//! `xil db-profile` — peak, average and minimum dBFS per MP3. Port of
//! `XILU010_db_profile.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Map, Value};
use xil_audio::ffmpeg;
use xil_audio::pcm::Pcm;
use xil_core::fsutil::{abspath, basename, relpath};
use xil_core::pycsv;
use xil_core::pyjson::{dumps, Style};
use xil_core::workspace::workspace_root;
use xil_core::{banner, log};

const CHUNK_MS: i64 = 500;
/// Below this a chunk counts as silence or dead air and is not a candidate
/// for the quietest-window measurement.
const SILENCE_FLOOR_DB: f64 = -96.0;
const DEFAULT_PATH: &str = "__sfx_default__";

#[derive(Parser)]
#[command(
    name = "xil-db-profile",
    about = "Profile MP3 audio levels: peak, average, and minimum dBFS"
)]
struct Args {
    /// MP3 file or directory to scan recursively (default: workspace SFX/ folder)
    #[arg(default_value = DEFAULT_PATH)]
    path: String,
    /// Write results to FILE as CSV in addition to logging
    #[arg(long, short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,
    /// Print absolute paths (default: relative to scan root)
    #[arg(long)]
    absolute: bool,
    /// Output a JSON array to stdout — no banner, safe to pipe to jq
    #[arg(long)]
    json: bool,
}

/// One file's measurements, in the key order the JSON and CSV use.
struct Record {
    path: String,
    duration_s: f64,
    peak_dbfs: f64,
    avg_dbfs: f64,
    min_dbfs: f64,
}

impl Record {
    fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("path".into(), Value::String(self.path.clone()));
        m.insert("duration_s".into(), num(self.duration_s));
        m.insert("peak_dBFS".into(), num(self.peak_dbfs));
        m.insert("avg_dBFS".into(), num(self.avg_dbfs));
        m.insert("min_dBFS".into(), num(self.min_dbfs));
        m
    }
}

/// A silent file profiles as `-inf`, which `json.dumps` writes as the
/// bare token `-Infinity`; `py_float` carries that through.
fn num(v: f64) -> Value {
    xil_core::pyjson::py_float(v)
}

/// Python's `round(x, 2)`: round-half-to-even on the decimal value.
fn round2(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    // Formatting then re-parsing gives the same half-to-even behaviour
    // CPython's float repr path produces.
    format!("{x:.2}").parse().unwrap_or(x)
}

/// Peak, RMS and quietest-500ms-window for one file.
pub fn profile_file(path: &Path) -> Result<(f64, f64, f64, f64), ffmpeg::AudioError> {
    let seg = ffmpeg::decode(path)?;
    let peak = seg.max_dbfs();
    let avg = seg.dbfs();
    let duration_s = seg.len_ms() as f64 / 1000.0;

    let mut levels: Vec<f64> = Vec::new();
    let total = seg.len_ms();
    let mut i = 0;
    while i < total {
        let chunk: Pcm = seg.slice_ms(i, i + CHUNK_MS);
        let d = chunk.dbfs();
        if d > SILENCE_FLOOR_DB {
            levels.push(d);
        }
        i += CHUNK_MS;
    }
    let min_db = levels.into_iter().fold(f64::INFINITY, f64::min);
    let min_db = if min_db.is_finite() {
        min_db
    } else {
        f64::NEG_INFINITY
    };

    Ok((
        round2(duration_s),
        round2(peak),
        round2(avg),
        round2(min_db),
    ))
}

/// `os.walk` order: this directory's files sorted, then subdirectories.
fn scan_mp3s(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let (mut files, mut dirs) = (Vec::new(), Vec::new());
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        if p.is_dir() {
            dirs.push(p);
        } else {
            files.push(p);
        }
    }
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for f in files {
        if basename(&f).to_lowercase().ends_with(".mp3") {
            out.push(abspath(&f));
        }
    }
    for d in dirs {
        walk(&d, out);
    }
}

/// Pad to `width` by character count (`f"{s:<*}"`).
fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

/// `%10.2f` on a value that may be infinite — Python prints `-inf`.
fn fmt_fixed(v: f64, width: usize) -> String {
    let s = if v.is_infinite() {
        (if v > 0.0 { "inf" } else { "-inf" }).to_string()
    } else {
        format!("{v:.2}")
    };
    let n = s.chars().count();
    if n >= width {
        s
    } else {
        format!("{}{s}", " ".repeat(width - n))
    }
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    let quiet = a.json;
    let resolved = if a.path == DEFAULT_PATH {
        workspace_root().join("SFX")
    } else {
        abspath(Path::new(&a.path))
    };

    let (mp3_paths, scan_root) = if resolved.is_file() {
        (
            vec![resolved.clone()],
            resolved.parent().map(Path::to_path_buf).unwrap_or_default(),
        )
    } else if resolved.is_dir() {
        if !quiet {
            log::info(&format!("Scanning {} for MP3 files…", resolved.display()));
        }
        (scan_mp3s(&resolved), resolved.clone())
    } else {
        log::error(&format!("Not a file or directory: {}", resolved.display()));
        return Ok(0);
    };

    if mp3_paths.is_empty() {
        if !quiet {
            log::info(&format!("No MP3 files found under {}", resolved.display()));
        }
        return Ok(0);
    }

    let mut records: Vec<Record> = Vec::new();
    for (i, p) in mp3_paths.iter().enumerate() {
        if !quiet {
            log::info(&format!(
                "[{}/{}] Profiling {}",
                i + 1,
                mp3_paths.len(),
                basename(p)
            ));
        }
        let (duration_s, peak, avg, min_db) = match profile_file(p) {
            Ok(v) => v,
            Err(e) => {
                log::warning(&format!("Skipping {} — {e}", p.display()));
                continue;
            }
        };
        let display = if a.absolute {
            p.display().to_string()
        } else {
            relpath(p, &scan_root).display().to_string()
        };
        records.push(Record {
            path: display,
            duration_s,
            peak_dbfs: peak,
            avg_dbfs: avg,
            min_dbfs: min_db,
        });
    }

    if records.is_empty() {
        return Ok(0);
    }

    if a.json {
        let arr = Value::Array(records.iter().map(|r| Value::Object(r.to_map())).collect());
        println!("{}", dumps(&arr, Style::INDENT2));
        return Ok(0);
    }

    let col_w = records
        .iter()
        .map(|r| r.path.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    let header = format!(
        "{}  {:>10}  {:>9}  {:>9}  {:>7}",
        pad("filename", col_w),
        "peak_dBFS",
        "avg_dBFS",
        "min_dBFS",
        "dur_s"
    );
    let rule = "-".repeat(header.chars().count());
    log::info(&header);
    log::info(&rule);
    for r in &records {
        log::info(&format!(
            "{}  {}  {}  {}  {}",
            pad(&r.path, col_w),
            fmt_fixed(r.peak_dbfs, 10),
            fmt_fixed(r.avg_dbfs, 9),
            fmt_fixed(r.min_dbfs, 9),
            fmt_fixed(r.duration_s, 7)
        ));
    }

    if let Some(out) = &a.output {
        let cols = ["path", "duration_s", "peak_dBFS", "avg_dBFS", "min_dBFS"];
        let rows: Vec<Map<String, Value>> = records.iter().map(Record::to_map).collect();
        let mut f = fs::File::create(out)?;
        pycsv::write_dicts(&mut f, &cols, &rows)?;
        if !quiet {
            log::info(&format!("Written: {}", out.display()));
        }
    }

    log::info(&format!("Profiled {} MP3 file(s)", records.len()));
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("db-profile");
    let a: Args = match super::parse_or_exit("xil-db-profile", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    if a.json {
        execute(&a)
    } else {
        let _banner = banner::begin(super::prog(), &super::argv_line(args));
        execute(&a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_two_places_matches_python() {
        assert_eq!(round2(1.005), 1.0, "1.005 is really 1.00499… in binary");
        assert_eq!(round2(-45.448237), -45.45);
        assert_eq!(round2(2.675), 2.67);
        assert_eq!(round2(f64::NEG_INFINITY), f64::NEG_INFINITY);
    }

    #[test]
    fn infinities_render_as_python_writes_them() {
        assert_eq!(dumps(&num(f64::NEG_INFINITY), Style::COMPACT), "-Infinity");
        assert_eq!(fmt_fixed(f64::NEG_INFINITY, 9), "     -inf");
        assert_eq!(fmt_fixed(-45.45, 9), "   -45.45");
        assert_eq!(fmt_fixed(-1234.5678, 3), "-1234.57", "too wide to pad");
    }

    #[test]
    fn scan_is_os_walk_order_and_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("sub")).unwrap();
        for f in ["b.mp3", "a.MP3", "c.wav", "sub/d.mp3"] {
            fs::write(r.join(f), "x").unwrap();
        }
        let names: Vec<String> = scan_mp3s(r)
            .iter()
            .map(|p| relpath(p, r).display().to_string())
            .collect();
        assert_eq!(names, vec!["a.MP3", "b.mp3", "sub/d.mp3"]);
    }

    #[test]
    fn a_silent_file_profiles_as_negative_infinity() {
        if !xil_audio::ffmpeg::available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("q.mp3");
        std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "quiet",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono:d=1",
            ])
            .arg(&p)
            .status()
            .unwrap();
        let (_dur, peak, avg, min_db) = profile_file(&p).unwrap();
        assert_eq!(peak, f64::NEG_INFINITY);
        assert_eq!(avg, f64::NEG_INFINITY);
        assert_eq!(
            min_db,
            f64::NEG_INFINITY,
            "every chunk is below the silence floor"
        );
    }
}
