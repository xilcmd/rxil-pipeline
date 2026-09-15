//! `xil use` — set or display the active show context. Port of `xil_use.py`.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use clap::Parser;
use xil_core::log;
use xil_core::workspace::{active_show, python_str, set_active_show, show_slug, workspace_root};

#[derive(Parser)]
#[command(
    name = "xil-use",
    about = "Set or display the active show context. Without arguments, lists all \
             shows discovered in the workspace (each configs/<slug>/project.json) \
             and marks the currently active show with an asterisk. With a show \
             name or slug, switches the active show; subsequent commands that \
             auto-detect the show (parse, produce, status, ...) use it until \
             changed again.",
    after_help = "The active show is stored in <workspace>/.active_show. A multi-word \
                  show name may be given unquoted; all arguments are joined with spaces."
)]
struct Args {
    /// show name or slug to activate (omit to list available shows)
    show: Vec<String>,
}

/// `[(slug, show_name)]` for every `configs/<slug>/project.json`, in
/// directory-name order (Python: `sorted(configs_dir.iterdir())`).
pub fn available_shows(root: &Path) -> Vec<(String, String)> {
    let configs = root.join("configs");
    let Ok(rd) = fs::read_dir(&configs) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    let mut out = Vec::new();
    for entry in entries {
        let pj = entry.join("project.json");
        if !(entry.is_dir() && pj.exists()) {
            continue;
        }
        let slug = entry
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = fs::read_to_string(&pj)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("show").map(python_str))
            .unwrap_or_else(|| slug.clone());
        out.push((slug, name));
    }
    out
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("use");
    let parsed: Args = match super::parse_or_exit("xil-use", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };

    let root = workspace_root();
    let shows = available_shows(&root);
    let current = active_show();

    if parsed.show.is_empty() {
        if shows.is_empty() {
            log::info("No shows found. Run 'xil init --show \"My Show\"' to create one.");
            return Ok(0);
        }
        log::info("Available shows (* = active):");
        for (slug, name) in &shows {
            let marker = if Some(slug) == current.as_ref() {
                "* "
            } else {
                "  "
            };
            log::info(&format!("  {marker}{name}  ({slug})"));
        }
        if let Some(cur) = &current {
            if !shows.iter().any(|(s, _)| s == cur) {
                log::warning(&format!(
                    "Active show '{cur}' has no project.json in configs/."
                ));
            }
        }
        return Ok(0);
    }

    let query = parsed.show.join(" ").trim().to_string();
    let query_slug = show_slug(&query);

    let matched = shows.iter().find(|(slug, name)| {
        *slug == query_slug || *slug == query || show_slug(name) == query_slug
    });

    let Some((slug, name)) = matched else {
        log::error(&format!(
            "No show matching '{query}'. Run 'xil use' to list available shows."
        ));
        return Ok(1);
    };

    set_active_show(slug)?;
    log::info(&format!("Active show: {name}  ({slug})"));
    Ok(0)
}

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_show(root: &Path, slug: &str, name: &str) {
        let d = root.join("configs").join(slug);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("project.json"), format!(r#"{{"show": "{name}"}}"#)).unwrap();
    }

    #[test]
    fn available_shows_sorted_by_slug_with_name() {
        let tmp = tempfile::tempdir().unwrap();
        make_show(tmp.path(), "the413", "THE 413");
        make_show(tmp.path(), "nightowls", "Night Owls");
        fs::create_dir_all(tmp.path().join("configs/noproject")).unwrap();
        assert_eq!(
            available_shows(tmp.path()),
            vec![
                ("nightowls".to_string(), "Night Owls".to_string()),
                ("the413".to_string(), "THE 413".to_string()),
            ]
        );
    }

    #[test]
    fn available_shows_falls_back_to_slug_on_bad_json() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("configs").join("broken");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("project.json"), "{not json").unwrap();
        assert_eq!(
            available_shows(tmp.path()),
            vec![("broken".to_string(), "broken".to_string())]
        );
    }

    #[test]
    fn available_shows_empty_without_configs_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(available_shows(tmp.path()).is_empty());
    }
}
