//! `xil remove-show` — delete every workspace file for a show. Port of
//! `XILU017_remove_show.py`.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use clap::Parser;
use xil_core::fsutil::glob_children;
use xil_core::log;
use xil_core::workspace::{show_slug, workspace_root};

use super::removal::{delete, fmt_bytes, input, rel_display, Item, Kind};

#[derive(Parser)]
#[command(
    name = "xil-remove-show",
    about = "Remove all workspace files for a given show. Shared assets (SFX/, logs/) are never touched.",
    after_help = "Examples:\n  xil remove-show mypodcast --dry-run\n  xil remove-show mypodcast --yes\n  \
                  xil remove-show \"My Podcast\" --dry-run\n  xil remove-show mypodcast --include-scripts --yes\n"
)]
struct Args {
    /// Show name or slug to remove (e.g. mypodcast or 'My Podcast')
    #[arg(value_name = "SHOW")]
    show: String,
    /// Show what would be removed without deleting anything
    #[arg(long, short = 'n')]
    dry_run: bool,
    /// Skip the confirmation prompt
    #[arg(long, short = 'y')]
    yes: bool,
    /// Also remove scripts/*_{slug}_*.md files whose filename contains the show slug (caution: source material)
    #[arg(long)]
    include_scripts: bool,
}

/// Slug for a show name or slug: direct hit on `configs/<slug>`, else a
/// `project.json` whose show name slugifies to it, else the slug as given.
pub fn resolve_slug(root: &Path, name_or_slug: &str) -> String {
    let candidate = show_slug(name_or_slug);
    if root
        .join("configs")
        .join(&candidate)
        .join("project.json")
        .exists()
        || root.join("configs").join(&candidate).is_dir()
    {
        return candidate;
    }
    if let Ok(rd) = fs::read_dir(root.join("configs")) {
        let mut dirs: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            let pj = d.join("project.json");
            if !pj.exists() {
                continue;
            }
            let name = fs::read_to_string(&pj)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v.get("show").and_then(|s| s.as_str().map(str::to_string)))
                .unwrap_or_default();
            if show_slug(&name) == candidate {
                return d
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or(candidate);
            }
        }
    }
    candidate
}

pub fn collect(root: &Path, slug: &str, include_scripts: bool) -> Vec<Item> {
    let mut items = Vec::new();
    for category in [
        "configs", "parsed", "stems", "daw", "masters", "cues", "posts",
    ] {
        let d = root.join(category).join(slug);
        if d.exists() {
            items.push(Item::dir(d, ""));
        }
    }
    for prefix in [format!("cast_{slug}_"), format!("sfx_{slug}_")] {
        for p in glob_children(root, &prefix, ".json") {
            items.push(Item::file(p, "legacy root"));
        }
    }
    let parsed_dir = root.join("parsed");
    if parsed_dir.is_dir() {
        for prefix in ["parsed", "annotated", "pre_splice_parsed", "orig_parsed"] {
            for p in glob_children(&parsed_dir, &format!("{prefix}_{slug}_"), ".json") {
                items.push(Item::file(p, "legacy parsed"));
            }
        }
    }
    if include_scripts {
        let scripts = root.join("scripts");
        if scripts.is_dir() {
            for p in glob_children(&scripts, "", ".md") {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if name.contains(&format!("_{slug}_")) || name.ends_with(&format!("_{slug}.md")) {
                    items.push(Item::file(p, "script"));
                }
            }
        }
    }
    let active = root.join(".active_show");
    if active.exists() {
        if let Ok(cur) = fs::read_to_string(&active) {
            if cur.trim() == slug {
                items.push(Item::file(active, ".active_show"));
            }
        }
    }
    items
}

/// Print the plan; return (total_files, total_bytes).
fn report(root: &Path, items: &[Item], slug: &str, dry_run: bool) -> (u64, u64) {
    let total_files: u64 = items.iter().map(Item::file_count).sum();
    let total_bytes: u64 = items.iter().map(Item::total_bytes).sum();
    if items.is_empty() {
        log::info(&format!(
            "Nothing found for show '{slug}' — workspace is already clean."
        ));
        return (0, 0);
    }
    let action = if dry_run { "Would remove" } else { "Removing" };
    log::info(&format!("{action} show '{slug}':"));
    log::info("");
    for item in items {
        let fc = item.file_count();
        if fc == 0 {
            continue;
        }
        let rel = rel_display(&item.path, root);
        let size = fmt_bytes(item.total_bytes());
        let tag = if item.label.is_empty() {
            String::new()
        } else {
            format!("  ({})", item.label)
        };
        match item.kind {
            Kind::Dir => log::info(&format!("  [DIR]  {rel}/  — {fc} file(s), {size}{tag}")),
            Kind::File => log::info(&format!("  [FILE] {rel}  — {size}{tag}")),
        }
    }
    let empty: Vec<_> = items
        .iter()
        .filter(|i| i.file_count() == 0 && i.path.exists())
        .collect();
    if !empty.is_empty() {
        log::info("");
        for item in empty {
            let rel = rel_display(&item.path, root);
            let kind = if item.kind == Kind::Dir {
                "[DIR] "
            } else {
                "[FILE]"
            };
            log::info(&format!("  {kind} {rel}  — empty"));
        }
    }
    log::info("");
    let suffix = if total_files > 0 {
        " across all matched items"
    } else {
        ""
    };
    log::info(&format!(
        "Total: {total_files} file(s), {}{suffix}",
        fmt_bytes(total_bytes)
    ));
    (total_files, total_bytes)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("remove-show");
    let a: Args = match super::parse_or_exit("xil-remove-show", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let root = workspace_root();
    let slug = resolve_slug(&root, &a.show);
    let items = collect(&root, &slug, a.include_scripts);
    let (total_files, total_bytes) = report(&root, &items, &slug, a.dry_run);
    let any_exists = items.iter().any(|i| i.path.exists());

    if a.dry_run {
        if total_files > 0 || any_exists {
            log::info("");
            log::info("Dry run — nothing deleted. Run without --dry-run to remove.");
        }
        return Ok(0);
    }
    if total_files == 0 && !any_exists {
        return Ok(0);
    }
    if !a.yes {
        log::info("");
        let confirm = input(&format!(
            "⚠️  This will permanently delete {total_files} file(s) ({}) for show \"{slug}\".\n    \
             Type \"{slug}\" to confirm (or Ctrl-C to abort): ",
            fmt_bytes(total_bytes)
        ));
        if confirm != slug {
            log::info("Aborted — input did not match. Nothing deleted.");
            return Ok(1);
        }
    }
    log::info("");
    let removed = delete(&items)?;
    log::info("");
    log::info(&format!("✓ Removed {removed} file(s) for show '{slug}'."));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relative path with forward slashes, so these expectations read the
    /// same on Windows as on the platforms that run the pipeline.
    fn slashed(p: &Path, root: &Path) -> String {
        rel_display(p, root).replace('\\', "/")
    }

    fn show(root: &Path, slug: &str, name: &str) {
        let d = root.join("configs").join(slug);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("project.json"), format!(r#"{{"show": "{name}"}}"#)).unwrap();
    }

    #[test]
    fn resolve_by_slug_name_or_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        show(r, "the413", "THE 413");
        show(r, "odd", "Night Owls");
        assert_eq!(resolve_slug(r, "the413"), "the413");
        assert_eq!(resolve_slug(r, "THE 413"), "the413");
        assert_eq!(resolve_slug(r, "Night Owls"), "odd");
        assert_eq!(resolve_slug(r, "No Such Show"), "nosuchshow");
    }

    #[test]
    fn collect_finds_dirs_legacy_files_scripts_and_active() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        show(r, "s", "S");
        fs::create_dir_all(r.join("parsed/s")).unwrap();
        fs::create_dir_all(r.join("scripts")).unwrap();
        fs::write(r.join("cast_s_S01E01.json"), "{}").unwrap();
        fs::write(r.join("parsed/orig_parsed_s_S01E01.json"), "{}").unwrap();
        fs::write(r.join("scripts/ep_s_S01E01.md"), "#").unwrap();
        fs::write(r.join("scripts/other.md"), "#").unwrap();
        fs::write(r.join(".active_show"), "s").unwrap();
        let labels: Vec<(String, &str)> = collect(r, "s", true)
            .iter()
            .map(|i| (slashed(&i.path, r), i.label))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("configs/s".to_string(), ""),
                ("parsed/s".to_string(), ""),
                ("cast_s_S01E01.json".to_string(), "legacy root"),
                (
                    "parsed/orig_parsed_s_S01E01.json".to_string(),
                    "legacy parsed"
                ),
                ("scripts/ep_s_S01E01.md".to_string(), "script"),
                (".active_show".to_string(), ".active_show"),
            ]
        );
        assert_eq!(collect(r, "s", false).len(), 5);
    }
}
