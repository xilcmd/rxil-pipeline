//! `xil remove-episode` — delete one episode's workspace files. Port of
//! `XILU018_remove_episode.py`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;

use clap::Parser;
use xil_core::log;
use xil_core::workspace::{resolve_slug, workspace_root};

use super::removal::{delete, fmt_bytes, input, rel_display, Item, Kind};

#[derive(Parser)]
#[command(
    name = "xil-remove-episode",
    about = "Remove all workspace files for a single episode. The source production script, masters/, and the \
             sfx edit journal are never touched. Shared assets (SFX/, logs/) are never touched.",
    after_help = "Examples:\n  xil remove-episode S01E01 --dry-run\n  xil remove-episode S01E01 --yes\n  \
                  xil remove-episode S01E01 --show \"Night Owls\" --dry-run\n"
)]
struct Args {
    /// Episode tag to remove (e.g. S01E01)
    #[arg(value_name = "TAG")]
    episode: String,
    /// Show name or slug (default: resolved from project.json / XIL_PROJECTROOT)
    #[arg(long, short = 's', value_name = "SHOW")]
    show: Option<String>,
    /// Show what would be removed without deleting anything
    #[arg(long, short = 'n')]
    dry_run: bool,
    /// Skip the confirmation prompt
    #[arg(long, short = 'y')]
    yes: bool,
}

pub fn collect(root: &Path, slug: &str, tag: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let mut file = |p: std::path::PathBuf, label: &'static str| items.push(Item::file(p, label));

    let cfg = root.join("configs").join(slug);
    file(cfg.join(format!("cast_{tag}.json")), "");
    file(cfg.join(format!("sfx_{tag}.json")), "");

    let psd = root.join("parsed").join(slug);
    for prefix in ["parsed", "orig_parsed", "pre_splice_parsed", "stem_verify"] {
        file(psd.join(format!("{prefix}_{tag}.json")), "");
    }
    file(psd.join(format!("parsed_{tag}.csv")), "");
    file(psd.join(format!("annotated_{tag}.csv")), "");

    let cues = root.join("cues").join(slug);
    file(cues.join(format!("cues_{tag}.md")), "");
    file(cues.join(format!("cues_manifest_{tag}.json")), "");

    // Directories are only listed when they exist.
    let mut dir = |p: std::path::PathBuf, label: &'static str| {
        if p.exists() {
            items.push(Item::dir(p, label));
        }
    };
    dir(root.join("stems").join(slug).join(tag), "");
    dir(root.join("daw").join(slug).join(tag), "");

    items.push(Item::file(
        root.join("posts")
            .join(slug)
            .join(format!("{tag}_posts.md")),
        "",
    ));

    let vs = root.join("voice_samples").join(tag);
    if vs.exists() {
        items.push(Item::dir(vs, ""));
    }

    items.push(Item::file(
        root.join(format!("cast_{slug}_{tag}.json")),
        "legacy root",
    ));
    items.push(Item::file(
        root.join(format!("sfx_{slug}_{tag}.json")),
        "legacy root",
    ));
    let legacy_psd = root.join("parsed");
    for prefix in ["parsed", "orig_parsed", "pre_splice_parsed", "annotated"] {
        items.push(Item::file(
            legacy_psd.join(format!("{prefix}_{slug}_{tag}.json")),
            "legacy parsed",
        ));
    }
    items.push(Item::file(
        legacy_psd.join(format!("parsed_{slug}_{tag}.csv")),
        "legacy parsed",
    ));

    let legacy_daw = root.join("daw").join(tag);
    if legacy_daw.exists() && legacy_daw != root.join("daw").join(slug).join(tag) {
        items.push(Item::dir(legacy_daw, "legacy daw"));
    }
    items.push(Item::file(
        root.join(format!("{slug}_{tag}_master.mp3")),
        "legacy master",
    ));

    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|i| seen.insert(i.path.clone()))
        .collect()
}

fn report(root: &Path, items: &[Item], slug: &str, tag: &str, dry_run: bool) -> (u64, u64) {
    let present: Vec<&Item> = items.iter().filter(|i| i.path.exists()).collect();
    let total_files: u64 = present.iter().map(|i| i.file_count()).sum();
    let total_bytes: u64 = present.iter().map(|i| i.total_bytes()).sum();
    if present.is_empty() {
        log::info(&format!(
            "Nothing found for episode '{tag}' (show '{slug}') — workspace is already clean."
        ));
        return (0, 0);
    }
    let action = if dry_run { "Would remove" } else { "Removing" };
    log::info(&format!("{action} episode '{tag}' (show '{slug}'):"));
    log::info("");
    for item in &present {
        let fc = item.file_count();
        let rel = rel_display(&item.path, root);
        let size = fmt_bytes(item.total_bytes());
        let tag_str = if item.label.is_empty() {
            String::new()
        } else {
            format!("  ({})", item.label)
        };
        match item.kind {
            Kind::Dir => log::info(&format!("  [DIR]  {rel}/  — {fc} file(s), {size}{tag_str}")),
            Kind::File => log::info(&format!("  [FILE] {rel}  — {size}{tag_str}")),
        }
    }
    log::info("");
    log::info(&format!(
        "Total: {total_files} file(s), {}",
        fmt_bytes(total_bytes)
    ));
    (total_files, total_bytes)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("remove-episode");
    let a: Args = match super::parse_or_exit("xil-remove-episode", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let root = workspace_root();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let tag = a.episode.clone();
    let items = collect(&root, &slug, &tag);
    let (total_files, total_bytes) = report(&root, &items, &slug, &tag, a.dry_run);

    if a.dry_run {
        if total_files > 0 {
            log::info("");
            log::info("Dry run — nothing deleted. Run without --dry-run to remove.");
        }
        return Ok(0);
    }
    if total_files == 0 {
        return Ok(0);
    }
    if !a.yes {
        log::info("");
        let confirm = input(&format!(
            "⚠️  This will permanently delete {total_files} file(s) ({}) for episode \"{tag}\" (show \"{slug}\").\n    \
             Type \"{tag}\" to confirm (or Ctrl-C to abort): ",
            fmt_bytes(total_bytes)
        ));
        if confirm != tag {
            log::info("Aborted — input did not match. Nothing deleted.");
            return Ok(1);
        }
    }
    log::info("");
    let removed = delete(&items)?;
    log::info("");
    log::info(&format!(
        "✓ Removed {removed} file(s) for episode '{tag}' (show '{slug}')."
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

    /// Relative path with forward slashes, so these expectations read the
    /// same on Windows as on the platforms that run the pipeline.
    fn slashed(p: &Path, root: &Path) -> String {
        rel_display(p, root).replace('\\', "/")
    }
    use std::fs;

    #[test]
    fn collect_lists_every_candidate_once_and_only_existing_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("stems/s/S01E01")).unwrap();
        fs::create_dir_all(r.join("daw/S01E01")).unwrap();
        let items = collect(r, "s", "S01E01");
        let rels: Vec<String> = items.iter().map(|i| slashed(&i.path, r)).collect();
        assert!(rels.contains(&"configs/s/cast_S01E01.json".to_string()));
        assert!(rels.contains(&"stems/s/S01E01".to_string()));
        assert!(
            !rels.contains(&"daw/s/S01E01".to_string()),
            "absent dirs are not listed"
        );
        assert!(
            rels.contains(&"daw/S01E01".to_string()),
            "legacy flat daw dir is listed"
        );
        assert_eq!(
            items
                .iter()
                .find(|i| slashed(&i.path, r) == "daw/S01E01")
                .unwrap()
                .label,
            "legacy daw"
        );
        let unique: HashSet<_> = rels.iter().collect();
        assert_eq!(unique.len(), rels.len());
        assert_eq!(items.iter().filter(|i| i.path.exists()).count(), 2);
    }
}
