//! The SFX edit journal — `sfx_<tag>_edits.jsonl` beside each SFX config.
//!
//! The timeline editor appends one record per save. A fresh skeleton from
//! `xil parse` wipes hand-tuned overrides, so the journal is replayed on top
//! to bring them back. Port of the journal half of `sfx_common.py`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::pyjson::{dumps, Style};

/// Fields the timeline editor may set on one cue. Anything else in a record
/// is ignored on replay.
pub const SFX_EDIT_FIELDS: [&str; 5] = [
    "volume_percentage",
    "ramp_in_seconds",
    "ramp_out_seconds",
    "play_duration",
    "source",
];

/// `sfx_X.json` → `sfx_X_edits.jsonl`.
pub fn sfx_edits_path(sfx_path: &Path) -> PathBuf {
    let s = sfx_path.to_string_lossy();
    // os.path.splitext: the extension is the last dot in the final component.
    let stem = match (s.rfind('.'), s.rfind('/')) {
        (Some(dot), Some(slash)) if dot > slash + 1 => &s[..dot],
        (Some(dot), None) if dot > 0 => &s[..dot],
        _ => &s[..],
    };
    PathBuf::from(format!("{stem}_edits.jsonl"))
}

/// Append one per-cue edit record. `None` values mean "clear this override"
/// and are preserved as JSON null so replay reproduces the save.
pub fn append_sfx_edit(
    sfx_path: &Path,
    key: &str,
    fields: &Map<String, Value>,
) -> std::io::Result<()> {
    let mut record = Map::new();
    record.insert("ts".into(), Value::String(utc_now()));
    record.insert("key".into(), Value::String(key.to_string()));
    record.insert("fields".into(), Value::Object(fields.clone()));
    append_record(sfx_path, &Value::Object(record))
}

/// Append one category-defaults record (`scope: "defaults"`, no `key`).
pub fn append_sfx_defaults_edit(
    sfx_path: &Path,
    fields: &Map<String, Value>,
) -> std::io::Result<()> {
    let mut record = Map::new();
    record.insert("ts".into(), Value::String(utc_now()));
    record.insert("scope".into(), Value::String("defaults".into()));
    record.insert("fields".into(), Value::Object(fields.clone()));
    append_record(sfx_path, &Value::Object(record))
}

fn append_record(sfx_path: &Path, record: &Value) -> std::io::Result<()> {
    let journal = sfx_edits_path(sfx_path);
    if let Some(d) = journal.parent() {
        if !d.as_os_str().is_empty() {
            fs::create_dir_all(d)?;
        }
    }
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&journal)?;
    writeln!(f, "{}", dumps(record, Style::COMPACT_UTF8))
}

/// `datetime.now(UTC).isoformat(timespec="seconds")` — `2026-09-11T18:30:00+00:00`.
fn utc_now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string()
}

/// Outcome of a replay: how many records were applied, and which keys did
/// not exist in the config (a renamed direction leaves a harmless orphan).
pub struct Replay {
    pub applied: usize,
    pub orphans: Vec<String>,
}

/// Reapply the journal onto the config at `sfx_path`, in journal order
/// (last write wins). `(0, [])` when no journal exists.
///
/// `warn` receives the message Python logs when a journaled `source`
/// overrides a different one — the operator has to see which asset won.
pub fn replay_sfx_edits(
    sfx_path: &Path,
    dry_run: bool,
    warn: &mut dyn FnMut(String),
) -> std::io::Result<Replay> {
    let journal = sfx_edits_path(sfx_path);
    if !journal.exists() {
        return Ok(Replay {
            applied: 0,
            orphans: Vec::new(),
        });
    }

    let text = fs::read_to_string(sfx_path)?;
    let mut data: Value = serde_json::from_str(&text).unwrap_or(Value::Object(Map::new()));
    if !data.is_object() {
        data = Value::Object(Map::new());
    }
    let obj = data.as_object_mut().expect("object");
    obj.entry("effects")
        .or_insert_with(|| Value::Object(Map::new()));

    let mut applied = 0usize;
    let mut orphans: Vec<String> = Vec::new();

    let journal_text = fs::read_to_string(&journal)?;
    for (i, raw) in journal_text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            warn(format!(
                "  Skipping malformed journal line {} in {}",
                i + 1,
                journal.display()
            ));
            continue;
        };

        if record.get("scope").and_then(Value::as_str) == Some("defaults") {
            let fields = record
                .get("fields")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let defaults = obj
                .entry("defaults")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .expect("defaults object");
            for (k, v) in fields {
                if v.is_null() {
                    defaults.remove(&k);
                } else {
                    defaults.insert(k, v);
                }
            }
            applied += 1;
            continue;
        }

        let (Some(key), Some(fields)) = (
            record
                .get("key")
                .and_then(Value::as_str)
                .map(str::to_string),
            record.get("fields").and_then(Value::as_object).cloned(),
        ) else {
            warn(format!(
                "  Skipping malformed journal line {} in {}",
                i + 1,
                journal.display()
            ));
            continue;
        };

        let effects = obj["effects"].as_object_mut().expect("effects object");
        if !effects.contains_key(&key) && !orphans.contains(&key) {
            orphans.push(key.clone());
        }
        let effect = effects
            .entry(key.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(effect) = effect.as_object_mut() else {
            applied += 1;
            continue;
        };
        let mut warnings = Vec::new();
        for field in SFX_EDIT_FIELDS {
            let Some(new) = fields.get(field) else {
                continue;
            };
            if new.is_null() {
                effect.remove(field);
                continue;
            }
            if field == "source" {
                if let Some(cur) = effect.get(field).filter(|c| !c.is_null()) {
                    if cur != new {
                        warnings.push(format!(
                            "  Journal overrides script hint for '{key}': {} -> {} \
                             (re-run xil sfx-hydrate --force to make the script win)",
                            cur.as_str().unwrap_or_default(),
                            new.as_str().unwrap_or_default()
                        ));
                    }
                }
            }
            effect.insert(field.to_string(), new.clone());
        }
        for w in warnings {
            warn(w);
        }
        applied += 1;
    }

    if applied > 0 && !dry_run {
        // Note the trailing newline: replay writes one, the fresh skeleton
        // in `xil parse` does not. Both behaviours are load-bearing for
        // byte-identical output.
        fs::write(sfx_path, format!("{}\n", dumps(&data, Style::INDENT2_UTF8)))?;
    }
    Ok(Replay { applied, orphans })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn journal_path_swaps_the_extension() {
        assert_eq!(
            sfx_edits_path(Path::new("configs/s/sfx_S01E01.json")),
            Path::new("configs/s/sfx_S01E01_edits.jsonl")
        );
        assert_eq!(
            sfx_edits_path(Path::new("sfx_X")),
            Path::new("sfx_X_edits.jsonl")
        );
        assert_eq!(
            sfx_edits_path(Path::new("a.b/sfx_X")),
            Path::new("a.b/sfx_X_edits.jsonl")
        );
    }

    #[test]
    fn replay_sets_clears_and_reports_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("sfx_S01E01.json");
        write(
            &cfg,
            r#"{"effects":{"BEAT":{"type":"silence","volume_percentage":50}}}"#,
        );
        let j = sfx_edits_path(&cfg);
        write(
            &j,
            concat!(
                r#"{"ts":"t","key":"BEAT","fields":{"volume_percentage":null,"play_duration":25}}"#,
                "\n",
                r#"{"ts":"t","key":"GONE","fields":{"source":"SFX/x.mp3"}}"#,
                "\n",
                "\n",
                r#"not json"#,
                "\n",
                r#"{"ts":"t","scope":"defaults","fields":{"music_volume_percentage":40}}"#,
                "\n",
            ),
        );
        let mut warns = Vec::new();
        let r = replay_sfx_edits(&cfg, false, &mut |m| warns.push(m)).unwrap();
        assert_eq!(r.applied, 3);
        assert_eq!(r.orphans, vec!["GONE"]);
        assert_eq!(warns.len(), 1, "the unparseable line warns");
        let out: Value = serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            out["effects"]["BEAT"],
            json!({"type": "silence", "play_duration": 25})
        );
        assert_eq!(out["effects"]["GONE"], json!({"source": "SFX/x.mp3"}));
        assert_eq!(out["defaults"], json!({"music_volume_percentage": 40}));
        assert!(fs::read_to_string(&cfg).unwrap().ends_with("}\n"));
    }

    #[test]
    fn replay_warns_when_journal_beats_a_script_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("sfx_a.json");
        write(
            &cfg,
            r#"{"effects":{"MUSIC: X":{"source":"SFX/from-script.mp3"}}}"#,
        );
        write(
            &sfx_edits_path(&cfg),
            "{\"key\":\"MUSIC: X\",\"fields\":{\"source\":\"SFX/from-journal.mp3\"}}\n",
        );
        let mut warns = Vec::new();
        replay_sfx_edits(&cfg, false, &mut |m| warns.push(m)).unwrap();
        assert_eq!(warns.len(), 1);
        assert!(
            warns[0].contains("SFX/from-script.mp3 -> SFX/from-journal.mp3"),
            "{}",
            warns[0]
        );
    }

    #[test]
    fn dry_run_leaves_the_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("sfx_a.json");
        let before = r#"{"effects":{"BEAT":{}}}"#;
        write(&cfg, before);
        write(
            &sfx_edits_path(&cfg),
            "{\"key\":\"BEAT\",\"fields\":{\"play_duration\":10}}\n",
        );
        let r = replay_sfx_edits(&cfg, true, &mut |_| {}).unwrap();
        assert_eq!(r.applied, 1);
        assert_eq!(fs::read_to_string(&cfg).unwrap(), before);
    }

    #[test]
    fn no_journal_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("sfx_a.json");
        write(&cfg, "{}");
        let r = replay_sfx_edits(&cfg, false, &mut |_| panic!("no warnings expected")).unwrap();
        assert_eq!((r.applied, r.orphans.len()), (0, 0));
    }

    #[test]
    fn append_writes_one_compact_line_per_record() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("sfx_a.json");
        let mut fields = Map::new();
        fields.insert("source".into(), Value::String("SFX/é.mp3".into()));
        fields.insert("volume_percentage".into(), Value::Null);
        append_sfx_edit(&cfg, "SFX: X", &fields).unwrap();
        append_sfx_edit(&cfg, "SFX: Y", &fields).unwrap();
        let text = fs::read_to_string(sfx_edits_path(&cfg)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(r#""key": "SFX: X""#), "{}", lines[0]);
        assert!(
            lines[0].contains("é.mp3"),
            "ensure_ascii=False is preserved"
        );
        assert!(lines[0].contains(r#""volume_percentage": null"#));
    }
}
