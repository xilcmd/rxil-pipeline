//! `xil gui` — the web dashboard. Port of `xil_gui.py`'s entry point; the
//! dashboard itself lives in the `xil-web` crate.
//!
//! Gradio's `--share` tunnel has no counterpart here; share the dashboard
//! through an SSH or Tailscale tunnel instead.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use xil_core::log;
use xil_web::StageCells;

use super::status::evaluate_episode;

#[derive(Parser)]
#[command(
    name = "xil-gui",
    about = "Launch the xil-pipeline web dashboard. Opens a browser UI with ten tabs: Setup (initialize a workspace / select the active show), Project (edit project.json), Episodes (workspace overview with parse/stems/DAW/master status), Run Stage (launch pipeline stages with live log streaming; dry-run on by default), Speakers, Cast Config and SFX Config (edit the respective JSON configs), Audio Preview (browse and play stems in the browser), Audio Grading (mark SFX library files accurate or rejected), and Timeline (interactive HTML timeline).",
    after_help = "Remote access: the server binds 127.0.0.1 by default. To reach it from\nanother machine, forward the port, e.g.:\n  ssh -L 7860:127.0.0.1:7860 <this-host>"
)]
struct Args {
    /// Port to listen on (default: 7860)
    #[arg(long, default_value_t = 7860)]
    port: u16,
    /// Host address to bind (default: 127.0.0.1)
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Append a timestamped session activity log to FILE
    #[arg(long, short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,
    /// Log detailed activity for the Timeline audio-properties dialog (SFX open/save requests) to stdout and logs/xil_YYYY-MM-DD.log
    #[arg(long, short = 'v')]
    verbose: bool,
}

/// `_stage_status(slug, tag)`: parse/stems/daw/master freshness from the
/// `xil status` engine. `✓` up to date, `⚠` stale, `○` not built; the stems
/// cell keeps its file count, and overall is the worst of the four.
pub fn stage_cells(slug: &str, tag: &str) -> StageCells {
    let stages = evaluate_episode(slug, tag, Path::new(""), false);
    let find = |name: &str| stages.iter().find(|s| s.name == name);
    let glyph = |status: &str| match status {
        "OK" => "✓",
        "STALE" => "⚠",
        _ => "○",
    };
    let g = |name: &str| {
        find(name)
            .map(|s| glyph(s.status))
            .unwrap_or("○")
            .to_string()
    };
    let produce = match find("stems") {
        Some(s) if s.status != "MISSING" => format!("{} {}", glyph(s.status), s.output_count),
        _ => "○".into(),
    };
    let shown: Vec<&str> = ["parsed", "stems", "daw", "master"]
        .iter()
        .filter_map(|n| find(n).map(|s| s.status))
        .collect();
    let overall = if shown.contains(&"MISSING") {
        "○ missing"
    } else if shown.contains(&"STALE") {
        "⚠ stale"
    } else {
        "✓ OK"
    };
    StageCells {
        parse: g("parsed"),
        produce,
        daw: g("daw"),
        master: g("master"),
        overall: overall.into(),
    }
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    if args.iter().any(|a| a == "--share") {
        eprintln!(
            "xil gui: --share was removed with the Python dashboard. \
             Forward the port instead, e.g. ssh -L 7860:127.0.0.1:7860 <this-host>"
        );
        return Ok(2);
    }
    let a: Args = match super::parse_or_exit("xil-gui", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    xil_web::print_workspace_banner();
    log::init("gui");
    if a.verbose {
        log::set_threshold(log::Level::Debug);
    }
    if let Some(path) = &a.output {
        xil_web::activity::open(path)?;
    }
    let xil_exe = std::env::current_exe()?;
    let state = xil_web::AppState::new(xil_exe, Arc::new(stage_cells));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(xil_web::serve(&a.host, a.port, state))?;
    Ok(0)
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, mtime: i64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(mtime, 0)).unwrap();
    }

    /// `_scaffold_fresh`: a parse→master chain with ascending mtimes.
    fn scaffold_fresh(root: &Path, slug: &str, tag: &str, stems: usize) {
        touch(&root.join(format!("scripts/{tag}_{slug}_v1.md")), 1001);
        touch(&root.join(format!("parsed/{slug}/parsed_{tag}.json")), 1002);
        for i in 0..stems {
            touch(
                &root.join(format!("stems/{slug}/{tag}/{:03}_intro_host.mp3", i + 1)),
                1003,
            );
        }
        touch(
            &root.join(format!("stems/{slug}/{tag}/{tag}_stem_manifest.json")),
            1003,
        );
        touch(
            &root.join(format!("daw/{slug}/{tag}/{tag}_layer_dialogue.wav")),
            1004,
        );
        touch(
            &root.join(format!("masters/{tag}_{slug}_2026-06-19.mp3")),
            1005,
        );
    }

    // One test, so the workspace variable is set once per process.
    #[test]
    fn stage_cells_track_freshness() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::env::set_var("XIL_PROJECTROOT", root);

        scaffold_fresh(root, "the413", "S01E01", 3);
        let st = stage_cells("the413", "S01E01");
        assert_eq!(
            (
                st.parse.as_str(),
                st.produce.as_str(),
                st.daw.as_str(),
                st.master.as_str(),
                st.overall.as_str()
            ),
            ("✓", "✓ 3", "✓", "✓", "✓ OK")
        );

        filetime::set_file_mtime(
            root.join("parsed/the413/parsed_S01E01.json"),
            filetime::FileTime::from_unix_time(9000, 0),
        )
        .unwrap();
        let st = stage_cells("the413", "S01E01");
        assert!(st.produce.starts_with('⚠'), "{st:?}");
        assert_eq!(st.overall, "⚠ stale");

        scaffold_fresh(root, "the413", "S01E02", 1);
        std::fs::remove_file(root.join("masters/S01E02_the413_2026-06-19.mp3")).unwrap();
        let st = stage_cells("the413", "S01E02");
        assert_eq!(
            (st.master.as_str(), st.overall.as_str()),
            ("○", "○ missing")
        );

        let st = stage_cells("the413", "S09E09");
        assert_eq!((st.parse.as_str(), st.produce.as_str()), ("○", "○"));
        std::env::remove_var("XIL_PROJECTROOT");
    }
}
