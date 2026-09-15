//! A bounded local copy of workspace audio, so playback of a NAS workspace
//! reads the network once, and the "play all" concatenation preview.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use xil_core::fsutil::basename;

use crate::episodes::load_stems;

/// `_AUDIO_CACHE_MAX_BYTES`.
pub const AUDIO_CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const COPY_CHUNK: usize = 4 * 1024 * 1024;

/// `_audio_cache_dir()`: `$XDG_CACHE_HOME` (or `~/.cache`)`/xil-gui/audio`.
pub fn audio_cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".cache")
        });
    let d = base.join("xil-gui").join("audio");
    let _ = fs::create_dir_all(&d);
    d
}

fn mtime_ns(m: &fs::Metadata) -> u128 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn touch(path: &Path) {
    let _ = filetime::set_file_mtime(path, filetime::FileTime::now());
}

/// `_evict_audio_cache(cache_dir, keep)`: delete oldest files until the cache
/// is under [`AUDIO_CACHE_MAX_BYTES`].
pub fn evict_audio_cache(cache_dir: &Path, keep: &Path) {
    let Ok(rd) = fs::read_dir(cache_dir) else {
        return;
    };
    let mut entries: Vec<(SystemTime, u64, PathBuf)> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.path();
            let m = e.metadata().ok()?;
            (m.is_file() && p != keep).then(|| (m.modified().unwrap_or(UNIX_EPOCH), m.len(), p))
        })
        .collect();
    let mut total: u64 = entries.iter().map(|e| e.1).sum();
    total += fs::metadata(keep).map(|m| m.len()).unwrap_or(0);
    entries.sort();
    for (_, size, path) in entries {
        if total <= AUDIO_CACHE_MAX_BYTES {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total -= size;
        }
    }
}

/// `_cached_audio_path(src)`: copy `src` into the cache (keyed by path, size
/// and mtime) and return the copy. Any I/O failure returns `src` unchanged.
pub fn cached_audio_path(src: &Path) -> PathBuf {
    let attempt = || -> std::io::Result<PathBuf> {
        let m = fs::metadata(src)?;
        let dir = audio_cache_dir();
        let abs = std::path::absolute(src)?;
        let key = hex(&Sha256::digest(
            format!("{}|{}|{}", abs.display(), m.len(), mtime_ns(&m)).as_bytes(),
        ));
        let ext = src
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let dest = dir.join(format!("{key}{ext}"));
        if dest.exists() {
            touch(&dest);
            return Ok(dest);
        }
        let part = dir.join(format!("{key}{ext}.part"));
        {
            let mut fin = fs::File::open(src)?;
            let mut fout = fs::File::create(&part)?;
            let mut buf = vec![0u8; COPY_CHUNK];
            loop {
                let n = fin.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                fout.write_all(&buf[..n])?;
            }
        }
        fs::rename(&part, &dest)?;
        evict_audio_cache(&dir, &dest);
        Ok(dest)
    };
    attempt().unwrap_or_else(|_| src.to_path_buf())
}

/// `_concatenate_stems(slug, tag, filter)`: every matching stem joined into
/// one cached MP3, keyed by the ordered `(path, size, mtime)` of its inputs so
/// a repeat click is instant and a re-produced stem rolls the key over.
///
/// Decoding goes through ffmpeg rather than pydub; this is a listening
/// preview, not a pipeline artifact, so it only has to sound the same.
pub fn concatenate_stems(slug: &str, tag: &str, filter: &str) -> Option<PathBuf> {
    let stems = load_stems(slug, tag, filter);
    if stems.is_empty() {
        return None;
    }
    let mut sig = vec![filter.to_string()];
    for (_, path) in &stems {
        let m = fs::metadata(path).ok()?;
        let abs = std::path::absolute(path).ok()?;
        sig.push(format!("{}|{}|{}", abs.display(), m.len(), mtime_ns(&m)));
    }
    let key = hex(&Sha256::digest(sig.join("\n").as_bytes()));
    let dir = audio_cache_dir();
    let out = dir.join(format!("concat_{key}.mp3"));
    if out.exists() {
        touch(&out);
        return Some(out);
    }
    let part = dir.join(format!("concat_{key}.part.mp3"));
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
    let mut graph = String::new();
    for (i, (_, path)) in stems.iter().enumerate() {
        cmd.arg("-i").arg(cached_audio_path(path));
        graph.push_str(&format!(
            "[{i}:a]aresample=44100,aformat=sample_fmts=s16:channel_layouts=stereo[a{i}];"
        ));
    }
    for i in 0..stems.len() {
        graph.push_str(&format!("[a{i}]"));
    }
    graph.push_str(&format!("concat=n={}:v=0:a=1[out]", stems.len()));
    cmd.args(["-filter_complex", &graph, "-map", "[out]", "-b:a", "128k"])
        .arg(&part);
    let ok = cmd.output().map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        let _ = fs::remove_file(&part);
        return None;
    }
    fs::rename(&part, &out).ok()?;
    evict_audio_cache(&dir, &out);
    Some(out)
}

/// The `/cache/...` URL for a file inside the audio cache, or `None` when the
/// path is elsewhere (a failed copy falls back to the workspace original).
pub fn cache_url(path: &Path) -> Option<String> {
    let dir = audio_cache_dir();
    path.parent()
        .filter(|p| *p == dir)
        .map(|_| format!("/cache/{}", basename(path)))
}
