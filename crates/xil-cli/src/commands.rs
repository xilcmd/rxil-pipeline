//! Subcommand registry. Mirrors `XIL_SCRIPT_COMMANDS` in the Python
//! `xil.py`: same names, same groups, same order, same help text.
//!
//! During the port every entry starts as `native: false` and is served by
//! the Python package through `delegate`. A command flips to `native: true`
//! only when its parity check in `tools/parity/suite.toml` is green.

use std::ffi::OsString;

/// Which help block a command is listed under.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Pipeline,
    Utility,
}

/// Signature every native command entry point shares.
///
/// `args` excludes the command name itself, exactly like the `sys.argv[1:]`
/// a Python `main()` sees after the dispatcher swaps argv.
pub type NativeFn = fn(args: &[OsString]) -> anyhow::Result<i32>;

pub struct CommandSpec {
    pub name: &'static str,
    /// Python module that implements the command. Used for the XILP/XILU tag
    /// in `--help` and for the delegation log line.
    pub py_module: &'static str,
    pub description: &'static str,
    pub group: Group,
    pub hint: &'static str,
    /// `Some` once the Rust implementation has reached parity.
    pub native: Option<NativeFn>,
}

macro_rules! cmd {
    ($name:literal, $module:literal, $desc:literal, $group:ident) => {
        cmd!($name, $module, $desc, $group, "")
    };
    ($name:literal, $module:literal, $desc:literal, $group:ident, $hint:literal) => {
        cmd!(@build None, $name, $module, $desc, $group, $hint)
    };
    // `native $path` marks a command whose Rust implementation has reached parity.
    (native $run:path, $name:literal, $module:literal, $desc:literal, $group:ident) => {
        cmd!(@build Some($run), $name, $module, $desc, $group, "")
    };
    (native $run:path, $name:literal, $module:literal, $desc:literal, $group:ident, $hint:literal) => {
        cmd!(@build Some($run), $name, $module, $desc, $group, $hint)
    };
    (@build $native:expr, $name:literal, $module:literal, $desc:literal, $group:ident, $hint:literal) => {
        CommandSpec {
            name: $name,
            py_module: $module,
            description: $desc,
            group: Group::$group,
            hint: $hint,
            native: $native,
        }
    };
}

/// Insertion order defines display order within each group.
#[rustfmt::skip] // one command per line reads as a table; keep it that way
pub const COMMANDS: &[CommandSpec] = &[
    cmd!(native crate::cmd::init::run, "init", "xil_pipeline.xil_init", "workspace scaffolding", Pipeline),
    cmd!(native crate::cmd::use_cmd::run, "use", "xil_pipeline.xil_use", "set / show the active show context", Utility, "(multi-show workspaces)"),
    cmd!(native crate::cmd::scan::run, "scan", "xil_pipeline.XILP000_script_scanner", "pre-flight script scanner", Pipeline),
    cmd!(native crate::cmd::parse::run, "parse", "xil_pipeline.XILP001_script_parser", "script parser", Pipeline),
    cmd!("cues", "xil_pipeline.XILP006_cues_ingester", "cues sheet ingestion", Pipeline),
    cmd!("produce", "xil_pipeline.XILP002_producer", "voice stem generation", Pipeline),
    cmd!("assemble", "xil_pipeline.XILP003_audio_assembly", "master audio assembly", Pipeline),
    cmd!("studio-onboard", "xil_pipeline.XILP004_studio_onboard", "ElevenLabs Studio project onboarding", Pipeline),
    cmd!("daw", "xil_pipeline.XILP005_daw_export", "DAW layer export", Pipeline),
    cmd!(native crate::cmd::migrate::run, "migrate", "xil_pipeline.XILP007_stem_migrator", "stem migration", Pipeline),
    cmd!(native crate::cmd::cleanup::run, "cleanup", "xil_pipeline.XILP008_stale_stem_cleanup", "stale stem cleanup", Pipeline),
    cmd!("import", "xil_pipeline.XILP010_studio_import", "Studio export import", Pipeline),
    cmd!(native crate::cmd::regen::run, "regen", "xil_pipeline.XILP009_script_regenerator", "script regeneration", Pipeline),
    cmd!("master", "xil_pipeline.XILP011_master_export", "final master MP3 export", Pipeline),
    cmd!("publish", "xil_pipeline.XILP012_publish", "social media post draft generator", Pipeline),
    cmd!("voices", "xil_pipeline.XILU001_discover_voices_T2S", "voice discovery", Utility, "(before parse/produce)"),
    cmd!(native crate::cmd::csv_join::run, "csv-join", "xil_pipeline.XILU003_csv_sfx_join", "CSV + SFX/cast annotation join", Utility, "(after parse)"),
    cmd!("sfx", "xil_pipeline.XILU002_generate_SFX", "standalone SFX generation", Utility, "(after cues/parse)"),
    cmd!("sample", "xil_pipeline.XILU004_sample_voices_T2S", "voice sample generation", Utility, "(after voices/cast config)"),
    cmd!(native crate::cmd::sfx_lib::run, "sfx-lib", "xil_pipeline.XILU005_discover_SFX", "SFX library discovery", Utility, "(any time)"),
    cmd!(native crate::cmd::splice::run, "splice", "xil_pipeline.XILU006_splice_parsed", "parsed JSON splice utility", Utility, "(advanced)"),
    cmd!(native crate::cmd::mp3_hash::run, "mp3-hash", "xil_pipeline.XILU007_mp3_hash", "recursive MP3 SHA-256 hash log", Utility, "(integrity / audit)"),
    cmd!(native crate::cmd::stem_log::run, "stem-log", "xil_pipeline.XILU008_stem_log_report", "parse daily logs → chronological stem generation CSV", Utility, "(integrity / audit)"),
    cmd!("gui", "xil_pipeline.xil_gui", "web dashboard (requires [gui] extra)", Utility, "(pip install xil-pipeline[gui])"),
    cmd!(native crate::cmd::migrate_workspace::run, "migrate-workspace", "xil_pipeline.XILU009_migrate_workspace", "migrate pre-0.1.8 workspace to normalized layout", Utility, "(run once per workspace)"),
    cmd!(native crate::cmd::db_profile::run, "db-profile", "xil_pipeline.XILU010_db_profile", "profile MP3 loudness: peak, average, and minimum dBFS", Utility, "(audio level analysis)"),
    cmd!(native crate::cmd::sfx_csv::run, "sfx-csv", "xil_pipeline.XILU011_sfx_csv", "flatten sfx_<tag>.json configs to CSV — one row per effect", Utility, "(debug / audit)"),
    cmd!(native crate::cmd::parsed_csv::run, "parsed-csv", "xil_pipeline.XILU012_parsed_csv", "export parsed_<tag>.json entries to CSV — one row per entry", Utility, "(debug / audit)"),
    cmd!(native crate::cmd::sfx_hydrate::run, "sfx-hydrate", "xil_pipeline.XILU013_sfx_hydrate", "write pipe-hint source fields from parsed JSON into the SFX config", Utility, "(after parse, before produce)"),
    cmd!(native crate::cmd::sfx_restore::run, "sfx-restore", "xil_pipeline.XILU020_sfx_restore", "reapply journaled timeline sound edits onto the SFX config", Utility, "(recover timeline sound edits)"),
    cmd!(native crate::cmd::sfx_impact::run, "sfx-impact", "xil_pipeline.XILU021_sfx_impact", "report which source-backed cues duration_seconds is clipping short", Utility, "(before changing clip durations)"),
    cmd!("sfx-match", "xil_pipeline.XILU022_sfx_match", "find existing library assets for cues whose source file is missing", Utility, "(when produce reports missing SFX sources)"),
    cmd!(native crate::cmd::episode_summary::run, "episode-summary", "xil_pipeline.XILU014_episode_summary", "write one-row-per-episode summary CSV (lines, words, TTS chars)", Utility, "(any time)"),
    cmd!("stem-verify", "xil_pipeline.XILU015_stem_verify", "scan stems folder → JSON report with file attributes and optional Whisper transcripts", Utility, "(after produce / import)"),
    cmd!("stem-compare", "xil_pipeline.XILU016_stem_compare", "cross-reference a stem-verify transcript report against the parsed script", Utility, "(after stem-verify)"),
    cmd!(native crate::cmd::remove_show::run, "remove-show", "xil_pipeline.XILU017_remove_show", "remove all workspace files for a show (--dry-run safe)", Utility, "(workspace management)"),
    cmd!(native crate::cmd::remove_episode::run, "remove-episode", "xil_pipeline.XILU018_remove_episode", "remove workspace files for one episode, preserving the source script (--dry-run safe)", Utility, "(workspace management)"),
    cmd!(native crate::cmd::status::run, "status", "xil_pipeline.XILU019_episode_status", "make-style staleness check of an episode's pipeline artifacts (report only)", Utility, "(workspace management)"),
];

pub fn find(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// The `XILP000` / `XILU001` identifier embedded in a module path, or "".
pub fn module_tag(module: &str) -> &str {
    let Some(start) = module.find("XIL") else {
        return "";
    };
    let rest = &module[start..];
    // "XIL", one of P/U, then digits.
    let mut end = 3;
    let bytes = rest.as_bytes();
    if bytes.len() < 4 || !matches!(bytes[3], b'P' | b'U') {
        return "";
    }
    end += 1;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == 4 {
        return "";
    }
    &rest[..end]
}

/// Render the command list exactly as the Python `_print_help` does.
pub fn help_text() -> String {
    let width = COMMANDS.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut out = String::from("Usage: xil <command> [args...]\n\n");
    for (label, group) in [
        ("Pipeline Stages (recommended order):", Group::Pipeline),
        ("Utilities:", Group::Utility),
    ] {
        out.push_str(label);
        out.push('\n');
        for c in COMMANDS.iter().filter(|c| c.group == group) {
            let tag = module_tag(c.py_module);
            let tag_str = if tag.is_empty() {
                " ".repeat(8)
            } else {
                format!("{tag:<8}")
            };
            let suffix = if c.hint.is_empty() {
                String::new()
            } else {
                format!("  {}", c.hint)
            };
            out.push_str(&format!(
                "  {:<width$}  {tag_str} {}{suffix}\n",
                c.name, c.description
            ));
        }
        out.push('\n');
    }
    out.push_str("Run 'xil <command> --help' for command-specific options.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_all_38_python_commands() {
        // len(XIL_SCRIPT_COMMANDS) in the Python xil.py at v0.3.2.
        assert_eq!(COMMANDS.len(), 38);
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<_> = COMMANDS.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), COMMANDS.len());
    }

    #[test]
    fn module_tag_extracts_xilp_and_xilu() {
        assert_eq!(module_tag("xil_pipeline.XILP000_script_scanner"), "XILP000");
        assert_eq!(module_tag("xil_pipeline.XILU022_sfx_match"), "XILU022");
        assert_eq!(module_tag("xil_pipeline.xil_init"), "");
        assert_eq!(module_tag("xil_pipeline.xil_gui"), "");
    }

    #[test]
    fn help_matches_python_layout_for_first_lines() {
        let text = help_text();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "Usage: xil <command> [args...]");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "Pipeline Stages (recommended order):");
        assert_eq!(
            lines[3],
            "  init                        workspace scaffolding"
        );
        assert_eq!(
            lines[4],
            "  scan               XILP000  pre-flight script scanner"
        );
        assert_eq!(
            lines.last().copied(),
            Some("Run 'xil <command> --help' for command-specific options.")
        );
    }
}
