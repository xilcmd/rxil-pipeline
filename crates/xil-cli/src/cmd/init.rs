//! `xil init` — scaffold a new workspace. Port of `xil_init.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, ValueEnum};
use serde_json::{json, Value};
use xil_core::fsutil::abspath;
use xil_core::log;
use xil_core::pyjson::{dumps, Style};
use xil_core::workspace::{set_active_show, show_slug, workspace_root};

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ContentType {
    Podcast,
    Audiobook,
    Drama,
    Special,
}

impl ContentType {
    fn name(self) -> &'static str {
        match self {
            ContentType::Podcast => "podcast",
            ContentType::Audiobook => "audiobook",
            ContentType::Drama => "drama",
            ContentType::Special => "special",
        }
    }

    fn speakers(self) -> &'static [(&'static str, &'static str)] {
        match self {
            ContentType::Podcast => &[
                ("HOST", "host"),
                ("CO-HOST", "co_host"),
                ("CALLER", "caller"),
                ("NARRATOR", "narrator"),
            ],
            ContentType::Audiobook => &[("NARRATOR", "narrator")],
            ContentType::Drama => &[
                ("NARRATOR", "narrator"),
                ("ALICE", "alice"),
                ("BOB", "bob"),
                ("CHARLIE", "charlie"),
                ("DEZ", "dez"),
            ],
            ContentType::Special => &[("HOST", "host"), ("NARRATOR", "narrator")],
        }
    }

    fn sample_tag(self) -> &'static str {
        match self {
            ContentType::Podcast | ContentType::Drama => "S01E01",
            ContentType::Audiobook => "V01C01",
            ContentType::Special => "SP001",
        }
    }

    fn script(self) -> &'static str {
        match self {
            ContentType::Podcast => PODCAST_SCRIPT,
            ContentType::Audiobook => AUDIOBOOK_SCRIPT,
            ContentType::Drama => DRAMA_SCRIPT,
            ContentType::Special => SPECIAL_SCRIPT,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "xil-init",
    about = "Scaffold a new xil-pipeline project workspace"
)]
struct Args {
    /// Target directory (default: XIL_PROJECTROOT if set, else current directory)
    directory: Option<String>,
    /// Show name for project.json (default: "Sample Show")
    #[arg(long, default_value = "Sample Show")]
    show: String,
    /// Content type: podcast (default), audiobook, drama, special
    #[arg(long = "type", value_enum, default_value_t = ContentType::Podcast)]
    content_type: ContentType,
    /// Season number for project.json and the sample script header
    #[arg(long, allow_hyphen_values = true)]
    season: Option<i64>,
    /// Season/arc title for project.json and the sample script header
    #[arg(long, value_name = "TITLE")]
    season_title: Option<String>,
    /// Use multi-show flat layout: project.json in configs/{slug}/, per-show category subdirs (default)
    #[arg(long, overrides_with = "no_flat")]
    flat: bool,
    /// Use legacy single-show layout: all dirs at workspace root
    #[arg(long, overrides_with = "flat")]
    no_flat: bool,
}

fn write_json(path: &Path, v: &Value) -> std::io::Result<()> {
    fs::write(path, format!("{}\n", dumps(v, Style::INDENT2)))
}

pub fn scaffold(
    directory: &Path,
    show_name: &str,
    content_type: ContentType,
    season: Option<i64>,
    season_title: Option<&str>,
    flat_layout: bool,
) -> anyhow::Result<()> {
    fs::create_dir_all(directory)?;
    let slug = show_slug(show_name);

    let mut project = serde_json::Map::new();
    project.insert("show".into(), Value::String(show_name.to_string()));
    project.insert(
        "type".into(),
        Value::String(content_type.name().to_string()),
    );
    if let Some(s) = season {
        project.insert("season".into(), Value::from(s));
    }
    if let Some(t) = season_title {
        project.insert("season_title".into(), Value::String(t.to_string()));
    }
    if content_type == ContentType::Audiobook {
        project.insert(
            "tag_format".into(),
            Value::String("V{volume:02d}C{chapter:02d}".into()),
        );
    }

    let project_path = if flat_layout {
        for shared in ["scripts", "SFX", "voice_samples"] {
            fs::create_dir_all(directory.join(shared))?;
        }
        for slug_dir in [format!("scripts/{slug}"), format!("SFX/{slug}")] {
            fs::create_dir_all(directory.join(slug_dir))?;
        }
        fs::create_dir_all(directory.join("configs").join(&slug))?;
        for category in ["daw", "stems", "masters", "parsed", "cues", "posts"] {
            fs::create_dir_all(directory.join(category).join(&slug))?;
        }
        directory.join("configs").join(&slug).join("project.json")
    } else {
        for sub in [
            "scripts",
            &format!("configs/{slug}"),
            "parsed",
            "stems",
            "SFX",
            "daw",
            "masters",
            "cues",
        ] {
            fs::create_dir_all(directory.join(sub))?;
        }
        directory.join("project.json")
    };

    if !project_path.exists() {
        write_json(&project_path, &Value::Object(project))?;
        log::info(&format!("  Created {}", project_path.display()));
    } else {
        log::info(&format!(
            "  Skipped {} (already exists)",
            project_path.display()
        ));
    }

    let speakers_path = directory.join("configs").join(&slug).join("speakers.json");
    if !speakers_path.exists() {
        let speakers: Vec<Value> = content_type
            .speakers()
            .iter()
            .map(|(display, key)| json!({"display": display, "key": key}))
            .collect();
        write_json(&speakers_path, &Value::Array(speakers))?;
        log::info(&format!("  Created {}", speakers_path.display()));
    } else {
        log::info(&format!(
            "  Skipped {} (already exists)",
            speakers_path.display()
        ));
    }

    let tag = content_type.sample_tag();
    let season_part = season.map(|s| format!(" Season {s}:")).unwrap_or_default();
    let arc_part = season_title
        .filter(|t| !t.is_empty())
        .map(|t| format!(" Arc: \"{t}\""))
        .unwrap_or_default();
    let script_path = if flat_layout {
        directory
            .join("scripts")
            .join(&slug)
            .join(format!("sample_{tag}.md"))
    } else {
        directory.join("scripts").join(format!("sample_{tag}.md"))
    };
    if !script_path.exists() {
        let body = content_type
            .script()
            .replace("{show}", show_name)
            .replace("{season_part}", &season_part)
            .replace("{arc_part}", &arc_part);
        fs::write(&script_path, body)?;
        log::info(&format!("  Created {}", script_path.display()));
    } else {
        log::info(&format!(
            "  Skipped {} (already exists)",
            script_path.display()
        ));
    }

    if flat_layout {
        set_active_show(&slug)?;
        log::info(&format!("  Active show set to: {slug}"));
    }
    Ok(())
}

/// The post-scaffold guide. `directory` is the raw CLI argument: Python
/// formats `None` into the `cd` prefix when it was omitted, and so do we.
fn print_getting_started(directory: Option<&str>, content_type: ContentType) {
    let dir = directory.unwrap_or("None");
    let cd_prefix = if dir != "." {
        format!("cd {dir} && ")
    } else {
        String::new()
    };
    let tag = content_type.sample_tag();
    log::info(&format!(
        "\nGetting Started\n===============\n\n1. Install the pipeline:\n   pip install xil-pipeline\n\n\
         2. Scan the sample script (pre-flight check):\n   {cd_prefix}xil-scan scripts/sample_{tag}.md\n\n\
         3. Parse the script into structured JSON:\n   {cd_prefix}xil-parse scripts/sample_{tag}.md --episode {tag}\n\n\
         4. Preview voice generation (no API key needed):\n   {cd_prefix}xil-produce --episode {tag} --dry-run\n\n\
         5. To use your own script:\n   - Edit configs/<slug>/speakers.json with your cast\n   - Write your script in scripts/\n   \
         - Set your ElevenLabs API key: export ELEVENLABS_API_KEY=your-key\n   - Run the pipeline stages in order (see README.md)\n"
    ));
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("init");
    let a: Args = match super::parse_or_exit("xil-init", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let flat_layout = !a.no_flat;
    let directory: PathBuf = match &a.directory {
        None => workspace_root(),
        Some(d) => abspath(Path::new(d)),
    };
    log::info(&format!(
        "\nScaffolding xil-pipeline workspace in: {}",
        directory.display()
    ));
    log::info(&format!(
        "Show: {}  Type: {}  Flat layout: {}\n",
        a.show,
        a.content_type.name(),
        if flat_layout { "True" } else { "False" }
    ));
    scaffold(
        &directory,
        &a.show,
        a.content_type,
        a.season,
        a.season_title.as_deref(),
        flat_layout,
    )?;
    print_getting_started(a.directory.as_deref(), a.content_type);
    Ok(0)
}

const PODCAST_SCRIPT: &str = r#"{show}{season_part} Episode 1: "Pilot"{arc_part}

CAST:
* HOST — the radio host
* CO-HOST — the co-host
* CALLER — a mysterious caller

===

COLD OPEN

SCENE 1: THE STUDIO

[AMBIENCE: Radio station studio, low hum of equipment]

HOST
Good evening, and welcome to the show. I'm your host.

CO-HOST
And I'm your co-host. We have a packed show tonight.

[SFX: Phone ringing]

HOST (picking up phone)
We have our first caller of the night. You're on the air.

CALLER (distorted, nervous)
Hi... I wasn't sure I should call, but something strange happened last night.

[BEAT]

HOST
Take your time. Tell us what happened.

CALLER
I was driving home when the radio cut out. Just static. And then... a voice.

[BEAT — 3 SECONDS]

===

ACT ONE

SCENE 2: THE INTERVIEW

[AMBIENCE: Same studio, quieter now]

HOST
We're going to dig into that after this break. Don't go anywhere.

CO-HOST
When we come back — the story that has our phones ringing off the hook.

[MUSIC: Upbeat podcast bumper]

[BEAT]

===

MID-EPISODE BREAK

HOST
You're listening to {show}. I'm your host, back with my co-host.

===

ACT TWO

SCENE 3: WRAP-UP

CO-HOST
Fascinating stuff. Thank you to everyone who called in tonight.

HOST
That's all for this episode. Until next time.

[MUSIC: Closing theme, fade out]

===

CLOSING

HOST
{show} is produced by [Your Name]. Subscribe wherever you get your podcasts.

===

END OF EPISODE
"#;

const AUDIOBOOK_SCRIPT: &str = r#"{show}{season_part} Episode 1: "Chapter One"{arc_part}

CAST:
* NARRATOR — the narrator

===

PROLOGUE

NARRATOR
Before we begin, a note from the author.

[BEAT — 2 SECONDS]

NARRATOR
The events in this story are entirely fictional. Any resemblance to actual
persons, living or dead, is purely coincidental.

[BEAT — 3 SECONDS]

===

CHAPTER ONE

NARRATOR
It began, as most things do, on an ordinary morning.

[BEAT]

NARRATOR
The sun rose over the hills, casting long shadows across the valley below.
Nothing about that day seemed remarkable. And yet, by nightfall, everything
would change.

[SFX: Birds chirping, distant wind]

NARRATOR
She stepped onto the porch with her coffee and looked out at the horizon.
Something was different. She couldn't say what — only that the light seemed
wrong, somehow. Too bright. Too still.

[BEAT — 2 SECONDS]

NARRATOR
She went back inside.

[BEAT — 3 SECONDS]

===

CHAPTER TWO

NARRATOR
Three days passed before anyone noticed she was gone.

[BEAT]

NARRATOR
The neighbor across the lane saw the mail piling up, the lights going dark
one by one. She mentioned it to her husband. He said to leave it alone.

[BEAT — 2 SECONDS]

NARRATOR
She didn't leave it alone.

[BEAT — 3 SECONDS]

===

END OF EPISODE
"#;

const DRAMA_SCRIPT: &str = r#"{show}{season_part} Episode 1: "Pilot"{arc_part}

CAST:
* NARRATOR — the narrator
* ALICE — protagonist
* BOB — antagonist
* CHARLIE — a bystander

===

ACT ONE

SCENE 1: THE STREET

[AMBIENCE: City street, distant traffic, wind]

NARRATOR
The city never sleeps. But tonight, it should have.

[BEAT]

ALICE (quietly)
I shouldn't be here.

BOB (behind her)
And yet, here you are.

[SFX: Footsteps on wet pavement]

ALICE
How long have you been following me?

BOB (calm)
Long enough to know you found the documents.

[BEAT — 2 SECONDS]

ALICE
I don't know what you're talking about.

BOB
We both know that's not true.

[SFX: Car horn, distant]

CHARLIE (interrupting)
Hey — is everything all right over there?

[BEAT]

BOB (forced smile)
Just old friends catching up.

===

ACT TWO

SCENE 2: THE ALLEY

[AMBIENCE: Narrow alley, dripping water, distant music]

NARRATOR
Alice ran. She didn't look back.

[SFX: Running footsteps, splashing]

ALICE (breathless, to herself)
I need to get to the bridge. I need to warn them.

[BEAT — 3 SECONDS]

NARRATOR
The bridge was three blocks away. Three very long blocks.

[MUSIC: Tense underscore, building]

[BEAT]

CHARLIE (stepping out of shadow)
I was hoping I'd find you first.

ALICE
Charlie? What are you doing here?

CHARLIE (serious)
Keeping you alive. Come with me.

[SFX: Door opening]

===

END OF EPISODE
"#;

const SPECIAL_SCRIPT: &str = r#"{show}{season_part} Episode 1: "Special Presentation"{arc_part}

CAST:
* HOST — the host
* NARRATOR — the narrator

===

INTRO

[MUSIC: Fanfare, brief]

HOST
Welcome to this special presentation of {show}.

NARRATOR
What follows is a one-time look at something we don't often discuss.

[BEAT — 2 SECONDS]

===

SEGMENT 1

HOST
Tonight we're exploring something that has fascinated people for generations.

NARRATOR
The question is simple. The answer, anything but.

[SFX: Ambient texture, subtle]

HOST
Let's begin.

[BEAT]

===

SEGMENT 2

HOST
Thank you for staying with us.

NARRATOR
We leave you with this thought: the most important stories are the ones
we tell ourselves.

[BEAT — 3 SECONDS]

HOST
Until next time.

===

OUTRO

[MUSIC: Closing theme]

HOST
This has been {show}. Thank you for listening.

===

END OF EPISODE
"#;

/// The clap definition behind `--help`, for man pages.
pub fn command() -> clap::Command {
    <Args as clap::CommandFactory>::command()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_scaffold_writes_project_speakers_script() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::env::set_var("XIL_PROJECTROOT", &ws);
        scaffold(
            &ws,
            "Night Owls",
            ContentType::Audiobook,
            Some(2),
            Some("Arc Two"),
            true,
        )
        .unwrap();
        let pj = fs::read_to_string(ws.join("configs/nightowls/project.json")).unwrap();
        assert_eq!(
            pj,
            "{\n  \"show\": \"Night Owls\",\n  \"type\": \"audiobook\",\n  \"season\": 2,\n  \"season_title\": \"Arc Two\",\n  \"tag_format\": \"V{volume:02d}C{chapter:02d}\"\n}\n"
        );
        let sp = fs::read_to_string(ws.join("configs/nightowls/speakers.json")).unwrap();
        assert_eq!(
            sp,
            "[\n  {\n    \"display\": \"NARRATOR\",\n    \"key\": \"narrator\"\n  }\n]\n"
        );
        let script = fs::read_to_string(ws.join("scripts/nightowls/sample_V01C01.md")).unwrap();
        assert!(script
            .starts_with("Night Owls Season 2: Episode 1: \"Chapter One\" Arc: \"Arc Two\"\n"));
        assert!(ws.join("posts/nightowls").is_dir());
        assert_eq!(
            fs::read_to_string(ws.join(".active_show")).unwrap(),
            "nightowls"
        );
        std::env::remove_var("XIL_PROJECTROOT");
    }

    #[test]
    fn legacy_scaffold_puts_project_at_root() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("legacy");
        scaffold(&ws, "Sample Show", ContentType::Podcast, None, None, false).unwrap();
        assert!(ws.join("project.json").exists());
        assert!(ws.join("scripts/sample_S01E01.md").exists());
        assert!(!ws.join("posts").exists());
    }
}
