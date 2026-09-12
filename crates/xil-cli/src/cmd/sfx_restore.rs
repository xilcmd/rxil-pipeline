//! `xil sfx-restore` — replay the timeline edit journal onto an episode's
//! SFX config. Port of `XILU020_sfx_restore.py`.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;
use xil_core::journal::{replay_sfx_edits, sfx_edits_path};
use xil_core::script::hints::py_repr;
use xil_core::workspace::{derive_paths, resolve_slug};
use xil_core::{banner, log};

const SCRIPT_NAME: &str = "XILU020_sfx_restore";

#[derive(Parser)]
#[command(
    name = "xil-sfx-restore",
    about = "Reapply journaled timeline sound edits (sfx_<tag>_edits.jsonl) onto the episode's SFX config — recovery for a cleared or regenerated sfx_<tag>.json."
)]
struct Args {
    /// Episode tag (e.g. S04E04)
    #[arg(long, required_unless_present = "tag", conflicts_with = "tag")]
    episode: Option<String>,
    /// Raw non-episodic tag (e.g. V01C03)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Override SFX config path
    #[arg(long)]
    sfx: Option<PathBuf>,
    /// Report what would be reapplied without writing
    #[arg(long)]
    dry_run: bool,
}

fn execute(a: &Args, tag: &str) -> anyhow::Result<i32> {
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let sfx_path = a
        .sfx
        .clone()
        .unwrap_or_else(|| derive_paths(&slug, tag)["sfx"].clone());

    if !sfx_path.exists() {
        log::error(&format!("SFX config not found: {}", sfx_path.display()));
        log::info(
            "Run `xil parse --episode TAG` first to generate it (the journal is reapplied automatically).",
        );
        return Ok(1);
    }
    let journal = sfx_edits_path(&sfx_path);
    if !journal.exists() {
        log::error(&format!("No edit journal found: {}", journal.display()));
        log::info(
            "The journal is created the first time a sound profile is saved in the GUI timeline editor.",
        );
        return Ok(1);
    }

    let replay = replay_sfx_edits(&sfx_path, a.dry_run, &mut |m| log::warning(&m))?;
    let action = if a.dry_run {
        "Would reapply"
    } else {
        "Reapplied"
    };
    log::info(&format!(
        "  {action} {} edit record(s) from {}",
        replay.applied,
        journal.display()
    ));
    for key in &replay.orphans {
        log::warning(&format!(
            "  Orphaned key (not in current effects): {}",
            py_repr(key)
        ));
    }
    if a.dry_run && replay.applied > 0 {
        log::info("  Re-run without --dry-run to apply changes.");
    }
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("sfx-restore");
    let a: Args = match super::parse_or_exit("xil-sfx-restore", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let _banner = banner::begin(SCRIPT_NAME, &super::argv_line(args));
    execute(&a, &tag)
}
