//! The Run Stage tab: command builders for each stage, extra-flag
//! validation, and subprocess jobs whose merged output streams to the page.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};

use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use xil_core::fsutil::{glob_children, sort_py};
use xil_core::workspace::workspace_root;

use crate::{activity, AppState, JobEvent};

/// Shell metacharacters refused in user-typed flags (`_SHELL_UNSAFE_RE`).
static SHELL_UNSAFE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[;|&$`()\[\]<>!\\\n\r]").unwrap());

/// `_sanitize_extra_flags(flags)`: shell-style split, then refuse any token
/// carrying a shell metacharacter.
pub fn sanitize_extra_flags(flags: &str) -> Result<Vec<String>, String> {
    let tokens = shlex::split(flags.trim())
        .ok_or_else(|| "Invalid flag syntax: No closing quotation".to_string())?;
    for tok in &tokens {
        if SHELL_UNSAFE.is_match(tok) {
            return Err(format!(
                "Unsafe character in flag argument: {}",
                xil_core::script::hints::py_repr(tok)
            ));
        }
    }
    Ok(tokens)
}

fn base(exe: &Path, stage: &str) -> Vec<String> {
    vec![exe.to_string_lossy().into_owned(), stage.to_string()]
}

fn opt(cmd: &mut Vec<String>, flag: &str, value: &str) {
    let v = value.trim();
    if !v.is_empty() {
        cmd.push(flag.into());
        cmd.push(v.into());
    }
}

fn flag(cmd: &mut Vec<String>, name: &str, on: bool) {
    if on {
        cmd.push(name.into());
    }
}

/// `_cmd_scan`.
pub fn cmd_scan(
    exe: &Path,
    slug: &str,
    script: &str,
    speakers: &str,
    json: bool,
) -> Result<Vec<String>, String> {
    if script.trim().is_empty() {
        return Err("Scan requires a script — select one from the dropdown.".into());
    }
    let mut cmd = base(exe, "scan");
    cmd.extend(["--show".into(), slug.into(), script.trim().into()]);
    opt(&mut cmd, "--speakers", speakers);
    flag(&mut cmd, "--json", json);
    Ok(cmd)
}

/// Options of the Parse sub-tab.
#[derive(Default)]
pub struct ParseOpts<'a> {
    pub script: &'a str,
    pub preview: i64,
    pub quiet: bool,
    pub debug: bool,
    pub stats: bool,
    pub speakers: &'a str,
}

/// `_cmd_parse`: a blank script falls back to the first `.md` under
/// `scripts/{slug}/`, then `scripts/`.
pub fn cmd_parse(exe: &Path, slug: &str, tag: &str, o: &ParseOpts) -> Result<Vec<String>, String> {
    let mut cmd = base(exe, "parse");
    if !o.script.trim().is_empty() {
        cmd.push(o.script.trim().into());
    } else {
        let root = workspace_root();
        let mut candidates: Vec<PathBuf> = if slug.is_empty() {
            Vec::new()
        } else {
            glob_children(&root.join("scripts").join(slug), "", ".md")
        };
        if candidates.is_empty() {
            candidates = glob_children(&root.join("scripts"), "", ".md");
        }
        sort_py(&mut candidates);
        let Some(first) = candidates.first() else {
            return Err("No script path given and no .md files found in scripts/".into());
        };
        cmd.push(first.to_string_lossy().into_owned());
    }
    cmd.extend(["--episode".into(), tag.into()]);
    if o.preview > 0 {
        cmd.extend(["--preview".into(), o.preview.to_string()]);
    }
    flag(&mut cmd, "--quiet", o.quiet);
    flag(&mut cmd, "--debug", o.debug);
    flag(&mut cmd, "--stats", o.stats);
    opt(&mut cmd, "--speakers", o.speakers);
    Ok(cmd)
}

/// Options of the Produce sub-tab.
#[derive(Default)]
pub struct ProduceOpts<'a> {
    pub dry_run: bool,
    pub backend: &'a str,
    pub gen_sfx: bool,
    pub gen_music: bool,
    pub gen_ambience: bool,
    pub local_only: bool,
    pub terse: bool,
    pub start_from: i64,
    pub stop_at: i64,
    pub chatterbox_python: &'a str,
    pub force: bool,
    pub sfx_backend: &'a str,
    pub mmaudio_python: &'a str,
    pub mmaudio_accept_nc: bool,
}

/// `_cmd_produce`.
pub fn cmd_produce(exe: &Path, tag: &str, o: &ProduceOpts) -> Vec<String> {
    let mut cmd = base(exe, "produce");
    cmd.extend(["--episode".into(), tag.into()]);
    flag(&mut cmd, "--dry-run", o.dry_run);
    if !o.backend.is_empty() && o.backend != "elevenlabs" {
        cmd.extend(["--backend".into(), o.backend.into()]);
    }
    flag(&mut cmd, "--gen-sfx", o.gen_sfx);
    flag(&mut cmd, "--gen-music", o.gen_music);
    flag(&mut cmd, "--gen-ambience", o.gen_ambience);
    flag(&mut cmd, "--local-only", o.local_only);
    flag(&mut cmd, "--terse", o.terse);
    if o.start_from > 0 {
        cmd.extend(["--start-from".into(), o.start_from.to_string()]);
    }
    if o.stop_at > 0 {
        cmd.extend(["--stop-at".into(), o.stop_at.to_string()]);
    }
    if o.backend == "chatterbox-turbo" {
        opt(&mut cmd, "--chatterbox-python", o.chatterbox_python);
    }
    if !o.sfx_backend.is_empty() && o.sfx_backend != "elevenlabs" {
        cmd.extend(["--sfx-backend".into(), o.sfx_backend.into()]);
    }
    if o.sfx_backend == "mmaudio" {
        flag(
            &mut cmd,
            "--mmaudio-accept-noncommercial",
            o.mmaudio_accept_nc,
        );
        opt(&mut cmd, "--mmaudio-python", o.mmaudio_python);
    }
    flag(&mut cmd, "--force", o.force);
    cmd
}

/// `_cmd_assemble`.
pub fn cmd_assemble(exe: &Path, tag: &str, gap_ms: i64, parsed: &str, output: &str) -> Vec<String> {
    let mut cmd = base(exe, "assemble");
    cmd.extend(["--episode".into(), tag.into()]);
    if gap_ms != 600 {
        cmd.extend(["--gap-ms".into(), gap_ms.to_string()]);
    }
    opt(&mut cmd, "--parsed", parsed);
    opt(&mut cmd, "--output", output);
    cmd
}

/// Options of the DAW sub-tab.
#[derive(Default)]
pub struct DawOpts<'a> {
    pub dry_run: bool,
    pub gap_ms: i64,
    pub timeline: bool,
    pub timeline_html: bool,
    pub macro_: bool,
    pub save_aup3: bool,
    pub output_dir: &'a str,
}

/// `_cmd_daw`.
pub fn cmd_daw(exe: &Path, tag: &str, o: &DawOpts) -> Vec<String> {
    let mut cmd = base(exe, "daw");
    cmd.extend(["--episode".into(), tag.into()]);
    flag(&mut cmd, "--dry-run", o.dry_run);
    if o.gap_ms != 600 {
        cmd.extend(["--gap-ms".into(), o.gap_ms.to_string()]);
    }
    flag(&mut cmd, "--timeline", o.timeline);
    flag(&mut cmd, "--timeline-html", o.timeline_html);
    flag(&mut cmd, "--macro", o.macro_);
    flag(&mut cmd, "--save-aup3", o.save_aup3);
    opt(&mut cmd, "--output-dir", o.output_dir);
    cmd
}

/// `_cmd_master`.
pub fn cmd_master(
    exe: &Path,
    tag: &str,
    dry_run: bool,
    output: &str,
    daw_dir: &str,
) -> Vec<String> {
    let mut cmd = base(exe, "master");
    cmd.extend(["--episode".into(), tag.into()]);
    flag(&mut cmd, "--dry-run", dry_run);
    opt(&mut cmd, "--output", output);
    opt(&mut cmd, "--daw-dir", daw_dir);
    cmd
}

/// `run_init`'s command: `xil init --show NAME --type T --flat --season N`.
pub fn cmd_init(
    exe: &Path,
    show: &str,
    content_type: &str,
    season: &str,
    season_title: &str,
) -> Vec<String> {
    let mut cmd = base(exe, "init");
    cmd.extend([
        "--show".into(),
        show.trim().into(),
        "--type".into(),
        content_type.into(),
        "--flat".into(),
        "--season".into(),
        if season.trim().is_empty() {
            "1".into()
        } else {
            season.trim().into()
        },
    ]);
    opt(&mut cmd, "--season-title", season_title);
    cmd
}

/// The `$ cmd` header every run log starts with.
pub fn header(cmd: &[String]) -> String {
    format!("$ {}\n\n", cmd.join(" "))
}

async fn pump<R: AsyncRead + Unpin>(reader: R, tx: UnboundedSender<JobEvent>) {
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                activity::log(&line);
                if tx.send(JobEvent::Line(line)).is_err() {
                    break;
                }
            }
            Ok(None) => break,
            // Not UTF-8: skip the line rather than lose the rest of the run.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(_) => break,
        }
    }
}

/// `_execute_cmd(cmd)`: start `cmd` in the workspace and register a job whose
/// stdout and stderr lines stream to `/jobs/{id}/stream`. Returns the job id.
pub fn start_job(state: &Arc<AppState>, cmd: Vec<String>) -> u64 {
    let id = state.job_id();
    let (tx, rx) = unbounded_channel();
    if let Ok(mut jobs) = state.jobs.lock() {
        jobs.insert(id, rx);
    }
    activity::log(&format!("CMD: {}", cmd.join(" ")));
    let job_state = state.clone();
    tokio::spawn(async move {
        let spawned = Command::new(&cmd[0])
            .args(&cmd[1..])
            .current_dir(workspace_root())
            .env("PYTHONUNBUFFERED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                activity::log(&format!("[ERROR] {e}"));
                let _ = tx.send(JobEvent::Exit(format!("[ERROR] {e}")));
                return;
            }
        };
        let out = child
            .stdout
            .take()
            .map(|o| tokio::spawn(pump(o, tx.clone())));
        let err = child
            .stderr
            .take()
            .map(|e| tokio::spawn(pump(e, tx.clone())));
        for task in [out, err].into_iter().flatten() {
            let _ = task.await;
        }
        let code = match child.wait().await {
            Ok(s) => s.code().unwrap_or(-1),
            Err(_) => -1,
        };
        activity::log(&format!("[exit {code}]"));
        // A stage can create episodes, scripts or configs.
        job_state.invalidate_choices();
        let _ = tx.send(JobEvent::Exit(format!("[exit {code}]")));
    });
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_flags_split_like_a_shell_and_refuse_metacharacters() {
        assert_eq!(sanitize_extra_flags("").unwrap(), Vec::<String>::new());
        assert_eq!(
            sanitize_extra_flags("--dry-run --gap-ms 400").unwrap(),
            ["--dry-run", "--gap-ms", "400"]
        );
        assert_eq!(
            sanitize_extra_flags("--output \"my file.mp3\"").unwrap(),
            ["--output", "my file.mp3"]
        );
        for bad in ["; rm -rf /", "a|b", "$(id)", "`id`", "x && y", "a>b", "!x"] {
            assert!(sanitize_extra_flags(bad).is_err(), "{bad}");
        }
        assert!(sanitize_extra_flags("\"unbalanced")
            .unwrap_err()
            .starts_with("Invalid flag syntax"));
    }

    #[test]
    fn produce_command_passes_only_non_default_options() {
        let exe = Path::new("xil");
        let o = ProduceOpts {
            backend: "elevenlabs",
            sfx_backend: "elevenlabs",
            ..Default::default()
        };
        assert_eq!(
            cmd_produce(exe, "S01E01", &o),
            ["xil", "produce", "--episode", "S01E01"]
        );
        let o = ProduceOpts {
            dry_run: true,
            backend: "chatterbox-turbo",
            chatterbox_python: " /venv/bin/python3 ",
            start_from: 5,
            stop_at: 9,
            sfx_backend: "mmaudio",
            mmaudio_accept_nc: true,
            mmaudio_python: "",
            force: true,
            ..Default::default()
        };
        assert_eq!(
            cmd_produce(exe, "S01E01", &o),
            [
                "xil",
                "produce",
                "--episode",
                "S01E01",
                "--dry-run",
                "--backend",
                "chatterbox-turbo",
                "--start-from",
                "5",
                "--stop-at",
                "9",
                "--chatterbox-python",
                "/venv/bin/python3",
                "--sfx-backend",
                "mmaudio",
                "--mmaudio-accept-noncommercial",
                "--force",
            ]
        );
    }

    #[test]
    fn scan_needs_a_script_and_daw_skips_default_gap() {
        let exe = Path::new("xil");
        assert!(cmd_scan(exe, "s", " ", "", false).is_err());
        assert_eq!(
            cmd_scan(exe, "s", "scripts/a.md", "sp.json", true).unwrap(),
            [
                "xil",
                "scan",
                "--show",
                "s",
                "scripts/a.md",
                "--speakers",
                "sp.json",
                "--json"
            ]
        );
        let o = DawOpts {
            gap_ms: 600,
            timeline_html: true,
            macro_: true,
            ..Default::default()
        };
        assert_eq!(
            cmd_daw(exe, "T", &o),
            ["xil", "daw", "--episode", "T", "--timeline-html", "--macro"]
        );
        assert_eq!(
            cmd_assemble(exe, "T", 400, "", " out.mp3 "),
            [
                "xil",
                "assemble",
                "--episode",
                "T",
                "--gap-ms",
                "400",
                "--output",
                "out.mp3"
            ]
        );
        assert_eq!(
            cmd_init(exe, " Night Owls ", "podcast", "", ""),
            [
                "xil",
                "init",
                "--show",
                "Night Owls",
                "--type",
                "podcast",
                "--flat",
                "--season",
                "1"
            ]
        );
    }
}
