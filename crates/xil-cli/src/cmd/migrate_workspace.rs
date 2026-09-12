//! `xil migrate-workspace` — move a pre-0.1.8 flat workspace to the
//! normalized layout. Port of `XILU009_migrate_workspace.py`.
//!
//! The Python calls `run_banner(...)` without entering it, so no banner is
//! ever printed. Faithfully reproduced: no banner here either.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use clap::Parser;
use regex::Regex;
use xil_core::fsutil::{abspath, basename, list_dir_raw, relpath};
use xil_core::log;
use xil_core::workspace::show_slug;

static CAST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^cast_([a-z0-9]+)_([A-Z0-9]+)\.json$").unwrap());
static SFX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^sfx_([a-z0-9]+)_([A-Z0-9]+)\.json$").unwrap());
static PARSED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^parsed_([a-z0-9]+)_([A-Z0-9]+)\.json$").unwrap());
static PARSED_CSV_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^parsed_([a-z0-9]+)_([A-Z0-9]+)\.csv$").unwrap());
static ANNOTATED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^parsed_([a-z0-9]+)_([A-Z0-9]+)_annotated\.csv$").unwrap());
static ORIG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^orig_parsed_([a-z0-9]+)_([A-Z0-9]+)\.json$").unwrap());
static PRE_SPLICE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^pre_splice_parsed_([a-z0-9]+)_([A-Z0-9]+)\.json$").unwrap());
static DAW_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Z0-9]+)$").unwrap());
static MASTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-z0-9]+)_([A-Z0-9]+)_master\.mp3$").unwrap());
static CUES_MD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^cues_([a-z0-9]+)_([A-Z0-9]+)\.md$").unwrap());
static CUES_MANIFEST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^cues_manifest_([A-Z0-9]+)\.json$").unwrap());

#[derive(Parser)]
#[command(
    name = "xil-migrate-workspace",
    about = "Migrate a pre-0.1.8 workspace to the normalized directory layout"
)]
struct Args {
    /// Workspace root directory (default: current directory)
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Preview moves without touching files (default: enabled for safety)
    #[arg(long)]
    dry_run: bool,
}

/// `glob.glob(os.path.join(dir, f"{prefix}*{suffix}"))` — raw directory
/// order, dotfiles excluded.
fn glob_raw(dir: &Path, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    list_dir_raw(dir)
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    !n.starts_with('.')
                        && n.starts_with(prefix)
                        && n.ends_with(suffix)
                        && n.len() >= prefix.len() + suffix.len()
                })
                .unwrap_or(false)
        })
        .collect()
}

fn names_raw(dir: &Path) -> Vec<String> {
    list_dir_raw(dir).iter().map(|p| basename(p)).collect()
}

fn infer_slug_from_project(workspace: &Path) -> Option<String> {
    let text = fs::read_to_string(workspace.join("project.json")).ok()?;
    let data: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(show_slug(
        data.get("show").and_then(|v| v.as_str()).unwrap_or(""),
    ))
}

fn infer_slug_from_tag(workspace: &Path, tag: &str) -> Option<String> {
    for slug_dir in list_dir_raw(&workspace.join("configs")) {
        if slug_dir.join(format!("cast_{tag}.json")).exists()
            && !basename(&slug_dir).starts_with('.')
        {
            return Some(basename(&slug_dir));
        }
    }
    for p in glob_raw(workspace, "cast_", &format!("_{tag}.json")) {
        if let Some(m) = CAST_RE.captures(&basename(&p)) {
            return Some(m[1].to_string());
        }
    }
    None
}

/// `(src, dst)` pairs for every legacy file found, as absolute paths.
pub fn discover_moves(workspace: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    fn push_move(
        moves: &mut Vec<(PathBuf, PathBuf)>,
        workspace: &Path,
        src_rel: String,
        dst_rel: String,
    ) {
        let (src, dst) = (
            abspath(&workspace.join(src_rel)),
            abspath(&workspace.join(dst_rel)),
        );
        if src.exists() && src != dst {
            moves.push((src, dst));
        }
    }

    if workspace.join("speakers.json").exists() {
        if let Some(slug) = infer_slug_from_project(workspace).filter(|s| !s.is_empty()) {
            push_move(
                &mut moves,
                workspace,
                "speakers.json".into(),
                format!("configs/{slug}/speakers.json"),
            );
        }
    }
    for p in glob_raw(workspace, "cast_", ".json") {
        if let Some(m) = CAST_RE.captures(&basename(&p)) {
            push_move(
                &mut moves,
                workspace,
                format!("cast_{}_{}.json", &m[1], &m[2]),
                format!("configs/{}/cast_{}.json", &m[1], &m[2]),
            );
        }
    }
    for p in glob_raw(workspace, "sfx_", ".json") {
        if let Some(m) = SFX_RE.captures(&basename(&p)) {
            push_move(
                &mut moves,
                workspace,
                format!("sfx_{}_{}.json", &m[1], &m[2]),
                format!("configs/{}/sfx_{}.json", &m[1], &m[2]),
            );
        }
    }

    let parsed_dir = workspace.join("parsed");
    if parsed_dir.is_dir() {
        for fname in names_raw(&parsed_dir) {
            if let Some(m) = PARSED_RE.captures(&fname) {
                push_move(
                    &mut moves,
                    workspace,
                    format!("parsed/parsed_{}_{}.json", &m[1], &m[2]),
                    format!("parsed/{}/parsed_{}.json", &m[1], &m[2]),
                );
            } else if let Some(m) = PARSED_CSV_RE.captures(&fname) {
                push_move(
                    &mut moves,
                    workspace,
                    format!("parsed/parsed_{}_{}.csv", &m[1], &m[2]),
                    format!("parsed/{}/parsed_{}.csv", &m[1], &m[2]),
                );
            } else if let Some(m) = ANNOTATED_RE.captures(&fname) {
                push_move(
                    &mut moves,
                    workspace,
                    format!("parsed/parsed_{}_{}_annotated.csv", &m[1], &m[2]),
                    format!("parsed/{}/annotated_{}.csv", &m[1], &m[2]),
                );
            } else if let Some(m) = ORIG_RE.captures(&fname) {
                push_move(
                    &mut moves,
                    workspace,
                    format!("parsed/orig_parsed_{}_{}.json", &m[1], &m[2]),
                    format!("parsed/{}/orig_parsed_{}.json", &m[1], &m[2]),
                );
            } else if let Some(m) = PRE_SPLICE_RE.captures(&fname) {
                push_move(
                    &mut moves,
                    workspace,
                    format!("parsed/pre_splice_parsed_{}_{}.json", &m[1], &m[2]),
                    format!("parsed/{}/pre_splice_parsed_{}.json", &m[1], &m[2]),
                );
            }
        }
    }

    // Directory moves bypass push_move: the Python appends them with no
    // exists/equality check of its own.
    let daw_dir = workspace.join("daw");
    if daw_dir.is_dir() {
        for entry in names_raw(&daw_dir) {
            let entry_path = daw_dir.join(&entry);
            if !entry_path.is_dir() || !DAW_RE.is_match(&entry) {
                continue;
            }
            if let Some(slug) = infer_slug_from_tag(workspace, &entry) {
                let dst_dir = daw_dir.join(&slug).join(&entry);
                if !dst_dir.exists() {
                    moves.push((entry_path, dst_dir));
                }
            }
        }
    }

    let masters_dir = workspace.join("masters");
    for (search_dir, is_root) in [(workspace.to_path_buf(), true), (masters_dir, false)] {
        if !search_dir.is_dir() && !is_root {
            continue;
        }
        for p in glob_raw(&search_dir, "", "_master.mp3") {
            if let Some(m) = MASTER_RE.captures(&basename(&p)) {
                let prefix = if is_root { "" } else { "masters/" };
                push_move(
                    &mut moves,
                    workspace,
                    format!("{prefix}{}_{}_master.mp3", &m[1], &m[2]),
                    format!("masters/{}/{}_master.mp3", &m[1], &m[2]),
                );
            }
        }
    }

    let cues_dir = workspace.join("cues");
    if cues_dir.is_dir() {
        for fname in names_raw(&cues_dir) {
            if let Some(m) = CUES_MD_RE.captures(&fname) {
                push_move(
                    &mut moves,
                    workspace,
                    format!("cues/cues_{}_{}.md", &m[1], &m[2]),
                    format!("cues/{}/cues_{}.md", &m[1], &m[2]),
                );
            } else if let Some(m) = CUES_MANIFEST_RE.captures(&fname) {
                let tag = m[1].to_string();
                if let Some(slug) = infer_slug_from_tag(workspace, &tag) {
                    push_move(
                        &mut moves,
                        workspace,
                        format!("cues/cues_manifest_{tag}.json"),
                        format!("cues/{slug}/cues_manifest_{tag}.json"),
                    );
                }
            }
        }
    }
    moves
}

/// `os.path.relpath(path)` — relative to the current directory.
fn rel(p: &Path) -> String {
    let cwd = env::current_dir().unwrap_or_default();
    relpath(p, &cwd).display().to_string()
}

fn execute_moves(moves: &[(PathBuf, PathBuf)], dry_run: bool) -> anyhow::Result<(usize, usize)> {
    let (mut moved, mut skipped) = (0, 0);
    for (src, dst) in moves {
        if dst.exists() {
            log::info(&format!("  SKIP   {} (already exists at target)", rel(dst)));
            skipped += 1;
            continue;
        }
        if dry_run {
            log::info(&format!("  MOVE   {}  →  {}", rel(src), rel(dst)));
        } else {
            if let Some(d) = dst.parent() {
                fs::create_dir_all(d)?;
            }
            fs::rename(src, dst)?;
            log::info(&format!("  MOVED  {}  →  {}", rel(src), rel(dst)));
        }
        moved += 1;
    }
    Ok((moved, skipped))
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("migrate-workspace");
    let a: Args = match super::parse_or_exit("xil-migrate-workspace", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let moves = discover_moves(&a.workspace);
    if moves.is_empty() {
        log::info("Nothing to migrate — workspace already uses the normalized layout.");
        return Ok(0);
    }
    let mode = if a.dry_run { "DRY RUN — " } else { "" };
    log::info(&format!(
        "\n{mode}Workspace migration plan ({} moves):\n",
        moves.len()
    ));
    let (moved, skipped) = execute_moves(&moves, a.dry_run)?;
    if a.dry_run {
        log::info(&format!(
            "\n{moved} file(s) would be moved, {skipped} already at target."
        ));
        log::info("Run without --dry-run to execute.");
    } else {
        log::info(&format!("\n{moved} file(s) moved, {skipped} skipped."));
        log::info("Migration complete. Run 'xil migrate-workspace --dry-run' to verify.");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_every_legacy_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path();
        for d in ["parsed", "daw/S01E01", "masters", "cues", "configs/old"] {
            fs::create_dir_all(w.join(d)).unwrap();
        }
        fs::write(w.join("project.json"), r#"{"show": "Old"}"#).unwrap();
        for f in [
            "speakers.json",
            "cast_old_S01E01.json",
            "sfx_old_S01E01.json",
            "parsed/parsed_old_S01E01.json",
            "parsed/parsed_old_S01E01.csv",
            "parsed/parsed_old_S01E01_annotated.csv",
            "parsed/orig_parsed_old_S01E01.json",
            "parsed/pre_splice_parsed_old_S01E01.json",
            "old_S01E01_master.mp3",
            "masters/old_S01E02_master.mp3",
            "cues/cues_old_S01E01.md",
            "cues/cues_manifest_S01E01.json",
            "configs/old/cast_S01E02.json",
        ] {
            fs::write(w.join(f), "x").unwrap();
        }
        let moves = discover_moves(w);
        let mut dsts: Vec<String> = moves.iter().map(|(_, d)| rel_to(d, w)).collect();
        dsts.sort();
        assert_eq!(
            dsts,
            vec![
                "configs/old/cast_S01E01.json",
                "configs/old/sfx_S01E01.json",
                "configs/old/speakers.json",
                "cues/old/cues_S01E01.md",
                "cues/old/cues_manifest_S01E01.json",
                "daw/old/S01E01",
                "masters/old/S01E01_master.mp3",
                "masters/old/S01E02_master.mp3",
                "parsed/old/annotated_S01E01.csv",
                "parsed/old/orig_parsed_S01E01.json",
                "parsed/old/parsed_S01E01.csv",
                "parsed/old/parsed_S01E01.json",
                "parsed/old/pre_splice_parsed_S01E01.json",
            ]
        );
    }

    fn rel_to(p: &Path, root: &Path) -> String {
        relpath(p, root).display().to_string()
    }

    #[test]
    fn nothing_to_move_in_normalized_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("parsed/show")).unwrap();
        fs::write(tmp.path().join("parsed/show/parsed_S01E01.json"), "{}").unwrap();
        assert!(discover_moves(tmp.path()).is_empty());
    }
}
