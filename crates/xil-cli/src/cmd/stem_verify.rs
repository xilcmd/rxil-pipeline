//! `xil stem-verify` — a JSON report of every stem's attributes and,
//! optionally, its Faster-Whisper transcript. Port of
//! `XILU015_stem_verify.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use clap::Parser;
use serde_json::{Map, Value};
use xil_core::fsutil::basename;
use xil_core::pyfmt::round_to;
use xil_core::pyjson::{dumps, py_float, Style};
use xil_core::workspace::{resolve_slug, resolve_venv_python, workspace_root};
use xil_core::{banner, log};
use xil_workers::Worker;

#[derive(Parser)]
#[command(
    name = "xil-stem-verify",
    about = "Scan a stems folder and produce a JSON report with file attributes and Whisper transcriptions."
)]
struct Args {
    /// Show slug (default: resolved from project.json or XIL_PROJECTROOT)
    #[arg(long, short = 's', value_name = "SLUG")]
    show: Option<String>,
    /// Episode tag, e.g. S01E01 (required unless --stems-dir is set)
    #[arg(long, short = 'e', value_name = "TAG")]
    episode: Option<String>,
    /// Override stems directory (default: <workspace>/stems/<slug>/<episode>/)
    #[arg(long, value_name = "DIR")]
    stems_dir: Option<String>,
    /// Output JSON path (default: <workspace>/parsed/<slug>/stem_verify_<episode>.json)
    #[arg(long, short = 'o', value_name = "FILE")]
    output: Option<String>,
    /// Path to venv-whisper Python executable (auto-detected if omitted)
    #[arg(long, value_name = "PATH")]
    whisper_python: Option<String>,
    /// Whisper model size: tiny|base|small|medium|large-v3|large-v3-turbo (default: large-v3-turbo)
    #[arg(long, default_value = "large-v3-turbo", value_name = "SIZE")]
    model: String,
    /// Language hint for Whisper. Use 'auto' for automatic detection (default: en)
    #[arg(long, default_value = "en", value_name = "LANG")]
    language: String,
    /// Whisper beam size (default: 5)
    #[arg(
        long,
        default_value_t = 5,
        value_name = "N",
        allow_negative_numbers = true
    )]
    beam_size: i64,
    /// Compute device for Whisper (default: cuda)
    #[arg(long, default_value = "cuda", value_parser = ["cuda", "cpu"])]
    device: String,
    /// Skip Whisper transcription; output file attributes only
    #[arg(long)]
    no_transcribe: bool,
}

/// `int(s)` as Python parses a stem prefix.
fn py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let clean: String = digits.chars().filter(|&c| c != '_').collect();
    if !clean.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    clean.parse::<i64>().ok().map(|n| if neg { -n } else { n })
}

/// `_parse_stem_filename` → `(seq, scene, speaker)`.
fn parse_stem_filename(filename: &str) -> (Option<i64>, Option<String>, Option<String>) {
    let name = if filename.to_lowercase().ends_with(".mp3") {
        let cut: Vec<char> = filename.chars().collect();
        cut[..cut.len() - 4].iter().collect::<String>()
    } else {
        filename.to_string()
    };
    let parts: Vec<&str> = name.splitn(3, '_').collect();
    if parts.len() == 3 {
        if let Some(seq) = py_int(parts[0]) {
            return (
                Some(seq),
                Some(parts[1].to_string()),
                Some(parts[2].to_string()),
            );
        }
    }
    (py_int(parts[0]), None, None)
}

/// `_mp3_metadata` → `(round(length, 3), bitrate // 1000)`, or both `None`.
fn mp3_metadata(path: &Path) -> (Option<f64>, Option<u64>) {
    match xil_audio::mpeg::info(path) {
        Ok(info) => (Some(round_to(info.length, 3)), Some(info.bitrate / 1000)),
        Err(_) => (None, None),
    }
}

/// `_WhisperClient` — strict about the first line being the ready message.
struct Whisper {
    worker: Worker,
}

impl Whisper {
    fn start(python: &str, script: &Path, device: &str, model: &str) -> anyhow::Result<Whisper> {
        let mut worker = Worker::spawn(
            Path::new(python),
            script,
            &[device.to_string(), model.to_string()],
        )?;
        let line = worker.read_line()?;
        let startup: Value = serde_json::from_str(&line)
            .map_err(|e| anyhow!("json.decoder.JSONDecodeError: {e}"))?;
        let ready = xil_workers::is_truthy(startup.get("ready"));
        if !ready {
            bail!(
                "Whisper worker failed to start: {}",
                dumps(&startup, Style::COMPACT)
            );
        }
        let field = |k: &str| match startup.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => "None".into(),
            Some(v) => dumps(v, Style::COMPACT),
        };
        log::info(&format!(
            "Whisper worker ready — model={} device={}",
            field("model"),
            field("device")
        ));
        Ok(Whisper { worker })
    }

    fn transcribe(
        &mut self,
        audio_path: &str,
        language: Option<&str>,
        beam_size: i64,
    ) -> anyhow::Result<Value> {
        let mut req = Map::new();
        req.insert("audio_path".into(), audio_path.into());
        req.insert(
            "language".into(),
            language.map(Value::from).unwrap_or(Value::Null),
        );
        req.insert("beam_size".into(), beam_size.into());
        self.worker.send(&Value::Object(req))?;
        let resp: Value = serde_json::from_str(&self.worker.read_line()?)?;
        if let Some(err) = resp.get("error") {
            let msg = match err {
                Value::String(s) => s.clone(),
                other => dumps(other, Style::COMPACT),
            };
            bail!(
                "Whisper error on {}: {msg}",
                basename(Path::new(audio_path))
            );
        }
        Ok(resp)
    }
}

fn process_files(
    files: &[PathBuf],
    mut whisper: Option<&mut Whisper>,
    language: Option<&str>,
    beam_size: i64,
) -> anyhow::Result<Vec<Value>> {
    let total = files.len();
    let mut records = Vec::new();
    for (i, mp3) in files.iter().enumerate() {
        let name = basename(mp3);
        log::info(&format!("[{}/{total}] {name}", i + 1));
        let (seq, scene, speaker) = parse_stem_filename(&name);
        if seq.is_none() {
            log::warning(&format!(
                "Unexpected filename format (skipping seq/scene/speaker parse): {name}"
            ));
        }
        let (duration, bitrate) = mp3_metadata(mp3);
        let digest = crate::cmd::mp3_hash::hash_file(mp3)?;
        let mut transcript = Value::Null;
        if let Some(w) = whisper.as_deref_mut() {
            match w.transcribe(&mp3.to_string_lossy(), language, beam_size) {
                Ok(resp) => {
                    let mut t = Map::new();
                    for k in ["text", "language", "language_probability", "segments"] {
                        match resp.get(k) {
                            Some(v) => {
                                t.insert(k.into(), v.clone());
                            }
                            None => {
                                transcript = Value::Null;
                                log::warning(&format!("Transcription failed for {name}: '{k}'"));
                                t.clear();
                                break;
                            }
                        }
                    }
                    if !t.is_empty() {
                        transcript = Value::Object(t);
                    }
                }
                Err(e) => log::warning(&format!("Transcription failed for {name}: {e}")),
            }
        }
        let mut r = Map::new();
        r.insert("filename".into(), name.clone().into());
        r.insert(
            "path".into(),
            fs::canonicalize(mp3)?.to_string_lossy().into_owned().into(),
        );
        r.insert("seq".into(), seq.map(Value::from).unwrap_or(Value::Null));
        r.insert(
            "scene".into(),
            scene.map(Value::from).unwrap_or(Value::Null),
        );
        r.insert(
            "speaker".into(),
            speaker.map(Value::from).unwrap_or(Value::Null),
        );
        r.insert("size_bytes".into(), fs::metadata(mp3)?.len().into());
        r.insert(
            "duration_seconds".into(),
            duration.map(py_float).unwrap_or(Value::Null),
        );
        r.insert(
            "bitrate_kbps".into(),
            bitrate.map(Value::from).unwrap_or(Value::Null),
        );
        r.insert("sha256".into(), digest.into());
        r.insert("transcript".into(), transcript);
        records.push(Value::Object(r));
    }
    Ok(records)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("stem-verify");
    let a: Args = match super::parse_or_exit("xil-stem-verify", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    execute(&a)
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    if a.episode.is_none() && a.stems_dir.is_none() {
        log::error("--episode or --stems-dir is required");
        return Ok(1);
    }
    let workspace = workspace_root();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let stems_dir = match a.stems_dir.as_ref().filter(|s| !s.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => workspace
            .join("stems")
            .join(&slug)
            .join(a.episode.clone().unwrap_or_default()),
    };
    if !stems_dir.is_dir() {
        log::error(&format!(
            "Stems directory not found: {}",
            stems_dir.display()
        ));
        return Ok(1);
    }
    let episode = a.episode.clone().filter(|e| !e.is_empty());
    let output_path = match (a.output.as_ref().filter(|s| !s.is_empty()), &episode) {
        (Some(o), _) => PathBuf::from(o),
        (None, Some(ep)) => workspace
            .join("parsed")
            .join(&slug)
            .join(format!("stem_verify_{ep}.json")),
        (None, None) => stems_dir.join("stem_verify_report.json"),
    };
    if let Some(parent) = output_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }

    let mut files: Vec<PathBuf> = fs::read_dir(&stems_dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let n = basename(p);
            matches!(n.rfind('.'), Some(i) if i > 0 && i < n.len() - 1 && n[i..].to_lowercase() == ".mp3")
        })
        .collect();
    xil_core::fsutil::sort_py(&mut files);
    if files.is_empty() {
        log::error(&format!("No MP3 files found in {}", stems_dir.display()));
        return Ok(1);
    }
    log::info(&format!(
        "Found {} MP3 files in {}",
        files.len(),
        stems_dir.display()
    ));

    let transcribe = !a.no_transcribe;
    let language = if a.language == "auto" {
        None
    } else {
        Some(a.language.as_str())
    };
    let records = if transcribe {
        let package_dir = crate::workers::python_package_dir();
        let Some(python) = resolve_venv_python(
            "venv-whisper",
            a.whisper_python.as_deref(),
            package_dir.as_deref(),
        ) else {
            log::error(
                "Cannot find venv-whisper Python. Pass --whisper-python PATH, set XIL_CODEROOT to the directory containing venv-whisper/, or create venv-whisper/ at the workspace or repo root. Use --no-transcribe to skip transcription.",
            );
            return Ok(1);
        };
        let script = package_dir
            .map(|d| xil_workers::worker_script(&d, "whisper_worker.py"))
            .ok_or_else(|| {
                anyhow!("cannot locate the xil_pipeline package that ships whisper_worker.py")
            })?;
        let mut whisper = Whisper::start(&python, &script, &a.device, &a.model)?;
        let records = process_files(&files, Some(&mut whisper), language, a.beam_size);
        whisper.worker.close()?;
        records?
    } else {
        process_files(&files, None, language, a.beam_size)?
    };

    let total_duration: f64 = records
        .iter()
        .map(|r| r["duration_seconds"].as_f64().unwrap_or(0.0))
        .sum();
    let mut report = Map::new();
    report.insert("show".into(), slug.into());
    report.insert(
        "episode".into(),
        episode.unwrap_or_else(|| basename(&stems_dir)).into(),
    );
    report.insert(
        "generated".into(),
        chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string()
            .into(),
    );
    report.insert(
        "stems_dir".into(),
        fs::canonicalize(&stems_dir)?
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    report.insert(
        "whisper_model".into(),
        if transcribe {
            a.model.clone().into()
        } else {
            Value::Null
        },
    );
    report.insert("file_count".into(), files.len().into());
    report.insert(
        "total_duration_seconds".into(),
        py_float(round_to(total_duration, 3)),
    );
    report.insert("files".into(), Value::Array(records));
    fs::write(&output_path, dumps(&Value::Object(report), Style::INDENT2))?;

    log::info(&format!("Written: {}", output_path.display()));
    log::info(&format!(
        "Total stems: {}  Total duration: {total_duration:.1}s",
        files.len()
    ));
    Ok(0)
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_names_parse_like_python() {
        assert_eq!(
            parse_stem_filename("003_cold-open_adam.mp3"),
            (Some(3), Some("cold-open".into()), Some("adam".into()))
        );
        assert_eq!(
            parse_stem_filename("001_Chapter 1.mp3"),
            (Some(1), None, None)
        );
        assert_eq!(parse_stem_filename("intro.MP3"), (None, None, None));
        assert_eq!(
            parse_stem_filename("010_act1_scene_host.mp3"),
            (Some(10), Some("act1".into()), Some("scene_host".into()))
        );
    }
}
