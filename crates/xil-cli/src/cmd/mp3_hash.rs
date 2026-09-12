//! `xil mp3-hash` — recursive MP3 SHA-256 hash log. Port of `XILU007_mp3_hash.py`.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::json;
use sha2::{Digest, Sha256};
use xil_core::fsutil::{abspath, relpath};
use xil_core::pyjson::{dumps, Style};
use xil_core::{banner, log};

#[derive(Parser)]
#[command(
    name = "xil-mp3-hash",
    about = "Recursively hash MP3 files and log <path> : <sha256>"
)]
struct Args {
    /// File to hash or directory to scan recursively (default: current directory)
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Write results to FILE in addition to logging
    #[arg(long, short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,
    /// Print absolute paths (default: paths relative to scan root)
    #[arg(long)]
    absolute: bool,
    /// Output a JSON array of {"path": ..., "sha256": ...} to stdout (no banner)
    #[arg(long)]
    json: bool,
}

/// Hex SHA-256 of a file, read in 64 KiB chunks.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// `os.walk` order: this directory's files sorted, then each subdirectory in
/// directory-listing order (not sorted — Python does not sort `dirnames`).
pub fn scan_mp3s(root: &Path) -> std::io::Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    walk(root, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) -> std::io::Result<()> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for e in fs::read_dir(dir)?.filter_map(Result::ok) {
        let p = e.path();
        if p.is_dir() {
            dirs.push(p);
        } else {
            files.push(p);
        }
    }
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for f in files {
        let is_mp3 = f
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_lowercase().ends_with(".mp3"))
            .unwrap_or(false);
        if is_mp3 {
            let full = abspath(&f);
            let digest = hash_file(&full)?;
            out.push((full, digest));
        }
    }
    for d in dirs {
        walk(&d, out)?;
    }
    Ok(())
}

fn execute(a: &Args) -> anyhow::Result<()> {
    let quiet = a.json;
    let root = abspath(&a.path);

    let records = if root.is_file() {
        vec![(root.clone(), hash_file(&root)?)]
    } else if root.is_dir() {
        if !quiet {
            log::info(&format!("Scanning {} for MP3 files…", root.display()));
        }
        scan_mp3s(&root)?
    } else {
        log::error(&format!("Not a file or directory: {}", root.display()));
        return Ok(());
    };

    if records.is_empty() {
        if !quiet {
            log::info(&format!("No MP3 files found under {}", root.display()));
        }
        return Ok(());
    }

    let display: Vec<(String, &str)> = records
        .iter()
        .map(|(p, d)| {
            let label = if a.absolute {
                p.display().to_string()
            } else {
                relpath(p, &root).display().to_string()
            };
            (label, d.as_str())
        })
        .collect();

    if a.json {
        let arr: Vec<_> = display
            .iter()
            .map(|(p, d)| json!({"path": p, "sha256": d}))
            .collect();
        println!("{}", dumps(&serde_json::Value::Array(arr), Style::INDENT2));
    } else {
        for (label, digest) in &display {
            log::info(&format!("{label} : {digest}"));
        }
    }

    if let Some(out) = &a.output {
        let mut f = File::create(out)?;
        for (label, digest) in &display {
            writeln!(f, "{label} : {digest}")?;
        }
        if !quiet {
            log::info(&format!("Written: {}", out.display()));
        }
    }

    if !quiet {
        log::info(&format!(
            "Hashed {} MP3 file(s) under {}",
            records.len(),
            root.display()
        ));
    }
    Ok(())
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    // Python configures logging at import time, so the log file exists in
    // every mode; only the banner is skipped for --json.
    log::init("mp3-hash");
    let a: Args = match super::parse_or_exit("xil-mp3-hash", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    if a.json {
        execute(&a)?;
    } else {
        let _banner = banner::begin(super::prog(), &super::argv_line(args));
        execute(&a)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_known_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.mp3");
        fs::write(&p, b"abc").unwrap();
        assert_eq!(
            hash_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn scan_finds_mp3s_case_insensitively_and_sorted_within_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("sub")).unwrap();
        for f in ["b.mp3", "a.MP3", "c.wav", "sub/d.mp3"] {
            fs::write(r.join(f), f).unwrap();
        }
        let names: Vec<String> = scan_mp3s(r)
            .unwrap()
            .iter()
            .map(|(p, _)| relpath(p, r).display().to_string())
            .collect();
        assert_eq!(names, vec!["a.MP3", "b.mp3", "sub/d.mp3"]);
    }
}
