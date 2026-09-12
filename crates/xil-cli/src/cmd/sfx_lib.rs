//! `xil sfx-lib` — inventory of the local SFX library from ID3 and MPEG
//! headers. Port of `XILU005_discover_SFX.py`.
//!
//! The `--api` path (ElevenLabs history over HTTP) is phase 5 work; until
//! then it hands off to Python before anything else happens.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Map, Value};
use xil_audio::mpeg;
use xil_audio::tags::read_text_tags;
use xil_core::fsutil::{basename, glob_recursive, relpath, sort_py};
use xil_core::pyfmt::{fixed, head, pad_right, round_to};
use xil_core::pyjson::{dumps, py_float, Style};
use xil_core::workspace::{code_root, workspace_root};
use xil_core::{banner, log};

const REFERENCE_DOC: &str = "claude-scriptwriter-reference.md";

#[derive(Parser)]
#[command(
    name = "xil-sfx-lib",
    about = "Discover personally generated Sound Effects from the ElevenLabs account or local SFX/ directory"
)]
struct Args {
    /// Query ElevenLabs /v1/sound-generation/history (requires sound_generation permission)
    #[arg(long, conflicts_with = "local")]
    api: bool,
    /// Scan local SFX/ directory only (no API key needed)
    #[arg(long)]
    local: bool,
    /// Local SFX directory to scan (default: <workspace>/SFX/)
    #[arg(long, value_name = "DIR")]
    sfx_dir: Option<String>,
    /// Case-insensitive substring filter on the prompt/filename
    #[arg(long, value_name = "TEXT")]
    search: Option<String>,
    /// (API mode) Paginate through the full account history; default: most recent 100
    #[arg(long)]
    all: bool,
    /// Print all fields for each record
    #[arg(long, short = 'v')]
    verbose: bool,
    /// Output results as a JSON array
    #[arg(long)]
    json: bool,
    /// Export SFX inventory JSON + scriptwriter reference doc to DIR (default: current directory)
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = ".")]
    export_kit: Option<String>,
}

/// `f"{seconds:.1f}s"`, or empty for `None`.
fn fmt_duration(seconds: Option<f64>) -> String {
    seconds
        .map(|s| format!("{}s", fixed(s, 1)))
        .unwrap_or_default()
}

/// `f"{bytes_ / 1024:.0f} KB"`.
fn fmt_size(bytes: u64) -> String {
    format!("{} KB", fixed(bytes as f64 / 1024.0, 0))
}

fn get_str<'a>(rec: &'a Map<String, Value>, key: &str) -> &'a str {
    rec.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `_read_local_record`: ID3 text frames plus MPEG duration/bit rate.
pub fn read_local_record(path: &Path, sfx_root: &Path) -> Map<String, Value> {
    let filename = basename(path);
    let size_bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let parent = path.parent().unwrap_or(Path::new(""));
    let rel_dir = relpath(parent, sfx_root).to_string_lossy().into_owned();
    let show = if rel_dir == "." {
        String::new()
    } else {
        rel_dir.split('/').next().unwrap_or("").to_string()
    };

    let tags = read_text_tags(path);
    let (duration, bitrate) = match mpeg::info(path) {
        Ok(i) => (
            py_float(round_to(i.length, 1)),
            Value::from(i.bitrate / 1000),
        ),
        Err(_) => (Value::Null, Value::Null),
    };

    let mut rec = Map::new();
    rec.insert("source".into(), "local".into());
    rec.insert("filename".into(), filename.into());
    rec.insert("path".into(), path.to_string_lossy().into_owned().into());
    rec.insert("show".into(), show.into());
    rec.insert("prompt".into(), tags.lyrics.into());
    rec.insert("title".into(), tags.title.into());
    rec.insert("artist".into(), tags.artist.into());
    rec.insert("duration_seconds".into(), duration);
    rec.insert("bitrate_kbps".into(), bitrate);
    rec.insert("size_bytes".into(), Value::from(size_bytes));
    rec.insert("date".into(), "".into());
    rec
}

/// One record per `.mp3` under `sfx_dir`, any depth, sorted by path.
pub fn fetch_local_records(sfx_dir: &Path) -> Vec<Map<String, Value>> {
    if !sfx_dir.is_dir() {
        log::warning(&format!("SFX directory not found: {}", sfx_dir.display()));
        return Vec::new();
    }
    // glob sorts by the path string, not by component.
    let mut paths = glob_recursive(sfx_dir, "", ".mp3");
    sort_py(&mut paths);
    paths
        .iter()
        .map(|p| read_local_record(p, sfx_dir))
        .collect()
}

fn print_verbose_local(rec: &Map<String, Value>) {
    log::info(&format!("  File           : {}", get_str(rec, "filename")));
    let show = get_str(rec, "show");
    log::info(&format!(
        "  Show           : {}",
        if show.is_empty() {
            "(shared pool)"
        } else {
            show
        }
    ));
    let prompt = get_str(rec, "prompt");
    log::info(&format!(
        "  Prompt         : {}",
        if prompt.is_empty() {
            "— (no prompt tag)"
        } else {
            prompt
        }
    ));
    let title = get_str(rec, "title");
    if !title.is_empty() {
        log::info(&format!("  Title          : {title}"));
    }
    if let Some(d) = rec.get("duration_seconds").and_then(Value::as_f64) {
        log::info(&format!("  Duration       : {}", fmt_duration(Some(d))));
    }
    if let Some(b) = rec.get("bitrate_kbps").and_then(Value::as_u64) {
        log::info(&format!("  Bitrate        : {b} kbps"));
    }
    log::info(&format!(
        "  Size           : {}",
        fmt_size(rec.get("size_bytes").and_then(Value::as_u64).unwrap_or(0))
    ));
    log::info("");
}

fn print_compact_local(rec: &Map<String, Value>) {
    let dur = fmt_duration(rec.get("duration_seconds").and_then(Value::as_f64));
    let size = fmt_size(rec.get("size_bytes").and_then(Value::as_u64).unwrap_or(0));
    let prompt = get_str(rec, "prompt");
    let prompt = if prompt.chars().count() > 72 {
        format!("{}…", head(prompt, 72))
    } else {
        prompt.to_string()
    };
    let meta = format!("{}  {}", pad_right(&dur, 6), pad_right(&size, 8));
    let show = get_str(rec, "show");
    let name = if show.is_empty() {
        get_str(rec, "filename").to_string()
    } else {
        format!("[{show}] {}", get_str(rec, "filename"))
    };
    log::info(&format!("  {}  {meta}", pad_right(&name, 38)));
    if !prompt.is_empty() {
        log::info(&format!("    {prompt}"));
    }
}

/// Filename → cheatsheet bucket, by prefix.
fn category(filename: &str) -> &'static str {
    let f = filename.to_lowercase();
    let b = f.as_bytes();
    if f.starts_with("ambience_") || f.starts_with("amb-") || f.starts_with("amb_") {
        return "AMBIENCE";
    }
    if f.starts_with("amb") && b.len() > 3 && b[3].is_ascii_alphabetic() {
        return "AMBIENCE";
    }
    if f.starts_with("music_") || f.starts_with("mus-") || f.starts_with("mus_") {
        return "MUSIC";
    }
    if f.starts_with("mus") && b.len() > 3 && b[3].is_ascii_alphabetic() {
        return "MUSIC";
    }
    if f.starts_with("beat") {
        return "BEAT";
    }
    "SFX"
}

/// `_write_pipe_hints_md`: returns the written path.
fn write_pipe_hints_md(
    records: &[Map<String, Value>],
    output_dir: &Path,
) -> std::io::Result<PathBuf> {
    let mut buckets: indexmap::IndexMap<&str, Vec<String>> = ["AMBIENCE", "MUSIC", "SFX", "BEAT"]
        .into_iter()
        .map(|k| (k, Vec::new()))
        .collect();
    let mut untitled: Vec<String> = Vec::new();
    for rec in records {
        let fnm = get_str(rec, "filename");
        let title = get_str(rec, "title").trim();
        if title.is_empty() {
            untitled.push(fnm.to_string());
        } else {
            buckets[category(fnm)].push(format!("[{title} | {fnm}]"));
        }
    }
    for lines in buckets.values_mut() {
        lines.sort();
    }
    untitled.sort();

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let titled_total: usize = buckets.values().map(Vec::len).sum();
    let mut out: Vec<String> = vec![
        "# SFX Pipe-Hint Cheatsheet".into(),
        format!(
            "# Generated {today} from {} assets ({titled_total} titled)",
            records.len()
        ),
        "# Copy lines verbatim into script directions — do NOT modify the filename.".into(),
        "# If a sound you need is not listed here, omit the pipe-hint entirely.".into(),
        String::new(),
    ];
    for cat in ["AMBIENCE", "MUSIC", "SFX", "BEAT"] {
        let entries = &buckets[cat];
        out.push(format!("## {cat} ({} titled)", entries.len()));
        out.push(String::new());
        out.extend(entries.iter().cloned());
        out.push(String::new());
    }
    if !untitled.is_empty() {
        out.push(format!(
            "## Untitled Assets — filename only ({} assets)",
            untitled.len()
        ));
        out.push("# These have no direction-text title tag.".into());
        out.push("# Match by filename keyword; use as bare source reference.".into());
        out.push(String::new());
        out.extend(untitled);
        out.push(String::new());
    }
    let hints_path = output_dir.join("sfx_pipe_hints.md");
    fs::write(&hints_path, out.join("\n"))?;
    Ok(hints_path)
}

/// `os.path.normpath` for the handful of shapes this command builds.
fn normpath(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let absolute = s.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in s.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        PathBuf::from(format!("/{joined}"))
    } else if joined.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(joined)
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// `shutil.copy2`: contents plus the modification time.
fn copy2(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::copy(src, dst)?;
    if let Ok(m) = fs::metadata(src).and_then(|m| m.modified()) {
        let f = fs::File::options().write(true).open(dst)?;
        f.set_modified(m)?;
    }
    Ok(())
}

/// Where Python looks first: `<package>/../../docs/<doc>`, which for the
/// editable install is the Python checkout's `docs/` directory. The doc
/// actually lives in `docs/guides/` there, so this lookup fails on both
/// sides and the CWD fallback decides; the path is still printed.
fn package_reference_path() -> PathBuf {
    match code_root() {
        Some(root) => root.join("docs").join(REFERENCE_DOC),
        None => {
            let exe = std::env::current_exe().unwrap_or_default();
            normpath(
                &exe.join("..")
                    .join("..")
                    .join("..")
                    .join("docs")
                    .join(REFERENCE_DOC),
            )
        }
    }
}

/// `export_kit`: `(json_path, hints_path, md_path)`; `md_path` is empty
/// when no reference doc was found.
fn export_kit(
    records: &[Map<String, Value>],
    output_dir: &Path,
) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    fs::create_dir_all(output_dir)?;
    let json_path = output_dir.join("sfx_inventory.json");
    let arr = Value::Array(records.iter().cloned().map(Value::Object).collect());
    fs::write(&json_path, dumps(&arr, Style::INDENT2) + "\n")?;
    let hints_path = write_pipe_hints_md(records, output_dir)?;

    let ref_src = normpath(&package_reference_path());
    let mut md_path = normpath(&output_dir.join(REFERENCE_DOC));
    if ref_src.exists() {
        if !same_file(&ref_src, &md_path) {
            copy2(&ref_src, &md_path)?;
        }
    } else {
        let cwd_ref = normpath(&Path::new("docs").join(REFERENCE_DOC));
        if cwd_ref.exists() {
            if !same_file(&cwd_ref, &md_path) {
                copy2(&cwd_ref, &md_path)?;
            } else {
                md_path = cwd_ref;
            }
        } else {
            log::warning(&format!(
                "Reference doc not found at {} or {}",
                ref_src.display(),
                cwd_ref.display()
            ));
            md_path = PathBuf::new();
        }
    }
    Ok((json_path, hints_path, md_path))
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    let sfx_dir = a
        .sfx_dir
        .clone()
        .unwrap_or_else(|| workspace_root().join("SFX").to_string_lossy().into_owned());
    let mut records = fetch_local_records(Path::new(&sfx_dir));
    let data_source = format!("local ({sfx_dir}/)");

    if let Some(q) = &a.search {
        let q = q.to_lowercase();
        records.retain(|r| {
            get_str(r, "prompt").to_lowercase().contains(&q)
                || get_str(r, "filename").to_lowercase().contains(&q)
        });
    }
    records.sort_by_key(|r| get_str(r, "filename").to_lowercase());

    if let Some(dir) = &a.export_kit {
        let (json_path, hints_path, md_path) = export_kit(&records, Path::new(dir))?;
        log::info(&format!(
            "\n--- Export kit ({} assets) ---\n",
            records.len()
        ));
        log::info(&format!("  JSON inventory : {}", json_path.display()));
        log::info(&format!(
            "  Pipe-hint cheatsheet : {}",
            hints_path.display()
        ));
        if !md_path.as_os_str().is_empty() {
            log::info(&format!("  Reference doc  : {}", md_path.display()));
        }
        log::info("");
        log::info("  Attach all three files to your Claude project as knowledge files.");
        return Ok(0);
    }

    if a.json {
        let arr = Value::Array(records.into_iter().map(Value::Object).collect());
        println!("{}", dumps(&arr, Style::INDENT2));
        return Ok(0);
    }

    log::info(&format!(
        "\n--- ElevenLabs Sound Effects  [{data_source}]  ({} items) ---\n",
        records.len()
    ));
    if records.is_empty() {
        log::info("  No sound-effect records found.");
        if let Some(q) = &a.search {
            log::info(&format!(
                "  (search filter: {})",
                xil_core::script::hints::py_repr(q)
            ));
        }
        return Ok(0);
    }

    if a.verbose {
        for rec in &records {
            print_verbose_local(rec);
        }
    } else {
        for rec in &records {
            print_compact_local(rec);
        }
        log::info("");
        let total: u64 = records
            .iter()
            .map(|r| r.get("size_bytes").and_then(Value::as_u64).unwrap_or(0))
            .sum();
        log::info(&format!(
            "  Total size: {} MB  ({} files)",
            fixed(total as f64 / (1024.0 * 1024.0), 1),
            records.len()
        ));
        log::info("");
        log::info("  Use --verbose for full details, --json for machine-readable output,");
        log::info("  --search <text> to filter, --local / --api to select data source.");
    }
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    // The ElevenLabs history endpoint is network code; Python keeps it
    // until phase 5.
    if args.iter().any(|a| a == "--api") {
        return crate::delegate::run("sfx-lib", args);
    }
    log::init("sfx-lib");
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-sfx-lib", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    execute(&a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_follow_filename_prefixes() {
        assert_eq!(category("AMBGras-field.mp3"), "AMBIENCE");
        assert_eq!(category("ambience_cafe.mp3"), "AMBIENCE");
        assert_eq!(
            category("amb1.mp3"),
            "SFX",
            "digit after amb is not a short code"
        );
        assert_eq!(category("MUSIC_theme.mp3"), "MUSIC");
        assert_eq!(category("MUSFolk-x.mp3"), "MUSIC");
        assert_eq!(category("beat.mp3"), "BEAT");
        assert_eq!(category("sfx_door.mp3"), "SFX");
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(fmt_duration(Some(2.35)), "2.4s");
        assert_eq!(fmt_duration(None), "");
        assert_eq!(fmt_size(1536), "2 KB");
        assert_eq!(fmt_size(1024 * 2 + 512), "2 KB", "half rounds to even");
        assert_eq!(
            normpath(Path::new("./kit/../kit/x.md")),
            PathBuf::from("kit/x.md")
        );
        assert_eq!(normpath(Path::new("/a/b/../../c")), PathBuf::from("/c"));
        assert_eq!(normpath(Path::new("./x")), PathBuf::from("x"));
    }

    #[test]
    fn records_carry_show_and_blank_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("SFX");
        fs::create_dir_all(root.join("myshow")).unwrap();
        fs::write(root.join("myshow").join("x.mp3"), b"ID3\x03\x00x").unwrap();
        fs::write(root.join("top.mp3"), b"").unwrap();
        fs::write(root.join("skip.MP3"), b"").unwrap();
        let recs = fetch_local_records(&root);
        let names: Vec<&str> = recs.iter().map(|r| get_str(r, "filename")).collect();
        assert_eq!(
            names,
            vec!["x.mp3", "top.mp3"],
            "path order, .MP3 not matched"
        );
        assert_eq!(get_str(&recs[0], "show"), "myshow");
        assert_eq!(get_str(&recs[1], "show"), "");
        assert!(recs[0]["duration_seconds"].is_null());
        assert_eq!(recs[0]["size_bytes"], 6);
        let keys: Vec<&str> = recs[0].keys().map(String::as_str).collect();
        assert_eq!(keys[0], "source");
        assert_eq!(keys[10], "date");
    }
}
