//! `xil csv-join` — annotate a parsed episode CSV with SFX and cast config
//! columns. Port of `XILU003_csv_sfx_join.py`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use xil_core::fsutil::abspath;
use xil_core::pycsv;
use xil_core::sfxlib::slugify_effect_key;
use xil_core::workspace::{derive_paths, resolve_slug};
use xil_core::{banner, log};

const INPUT_COLS: [&str; 10] = [
    "md_line_num",
    "md_raw",
    "seq",
    "type",
    "section",
    "scene",
    "speaker",
    "direction",
    "text",
    "direction_type",
];

const SFX_COLS: [&str; 7] = [
    "sfx_type",
    "sfx_prompt",
    "sfx_duration_seconds",
    "sfx_prompt_influence",
    "sfx_loop",
    "sfx_slug",
    "sfx_matched",
];

const CAST_COLS: [&str; 6] = [
    "cast_full_name",
    "cast_voice_id",
    "cast_pan",
    "cast_filter",
    "cast_role",
    "cast_matched",
];

#[derive(Parser)]
#[command(
    name = "xil-csv-join",
    about = "Annotate a parsed episode CSV with SFX and cast config data."
)]
struct Args {
    /// Episode tag (e.g. S02E03) — derives default input/output paths
    #[arg(long, required_unless_present = "tag", conflicts_with = "tag")]
    episode: Option<String>,
    /// Raw tag for non-episodic content (e.g. V01C03, D01)
    #[arg(long)]
    tag: Option<String>,
    /// Show name override (default: from project.json)
    #[arg(long)]
    show: Option<String>,
    /// Override input CSV path
    #[arg(long = "csv", value_name = "CSV_PATH")]
    csv_path: Option<PathBuf>,
    /// Override SFX JSON path
    #[arg(long = "sfx", value_name = "SFX_PATH")]
    sfx_path: Option<PathBuf>,
    /// Override cast JSON path
    #[arg(long = "cast", value_name = "CAST_PATH")]
    cast_path: Option<PathBuf>,
    /// Override output CSV path
    #[arg(long = "output", value_name = "OUT_PATH")]
    out_path: Option<PathBuf>,
}

/// One input row as `csv.DictReader` yields it: an absent key is Python's
/// `None`, which every consumer here treats like an empty string.
type Row = IndexMap<String, String>;

fn row_get<'a>(row: &'a Row, key: &str) -> &'a str {
    row.get(key).map(String::as_str).unwrap_or("")
}

fn blank(cols: &[&str], matched_col: &str) -> Map<String, Value> {
    let mut m = Map::new();
    for c in cols {
        m.insert((*c).into(), Value::String(String::new()));
    }
    m.insert(matched_col.into(), Value::String("FALSE".into()));
    m
}

/// SFX annotation columns for one row. Only `direction` rows can match.
fn join_sfx(
    row: &Row,
    effects: &Map<String, Value>,
    default_influence: &Value,
) -> Map<String, Value> {
    if row_get(row, "type") != "direction" {
        return blank(&SFX_COLS, "sfx_matched");
    }
    let text = row_get(row, "text");
    let Some(entry) = effects.get(text).and_then(Value::as_object) else {
        return blank(&SFX_COLS, "sfx_matched");
    };
    let influence = match entry.get("prompt_influence") {
        None | Some(Value::Null) => default_influence.clone(),
        Some(v) => v.clone(),
    };
    let mut m = Map::new();
    m.insert(
        "sfx_type".into(),
        entry
            .get("type")
            .cloned()
            .unwrap_or(Value::String("sfx".into())),
    );
    m.insert("sfx_prompt".into(), super::get_or_blank(entry, "prompt"));
    m.insert(
        "sfx_duration_seconds".into(),
        super::get_or_empty(entry, "duration_seconds"),
    );
    m.insert(
        "sfx_prompt_influence".into(),
        if influence.is_null() {
            Value::String(String::new())
        } else {
            influence
        },
    );
    m.insert(
        "sfx_loop".into(),
        Value::String(
            if entry.get("loop").is_some_and(super::truthy) {
                "TRUE"
            } else {
                ""
            }
            .into(),
        ),
    );
    m.insert("sfx_slug".into(), Value::String(slugify_effect_key(text)));
    m.insert("sfx_matched".into(), Value::String("TRUE".into()));
    m
}

/// Cast annotation columns for one row. Only `dialogue` rows carry a speaker.
fn join_cast(row: &Row, cast: &Map<String, Value>) -> Map<String, Value> {
    if row_get(row, "type") != "dialogue" {
        return blank(&CAST_COLS, "cast_matched");
    }
    let speaker = row_get(row, "speaker");
    if speaker.is_empty() {
        return blank(&CAST_COLS, "cast_matched");
    }
    let Some(member) = cast.get(speaker).and_then(Value::as_object) else {
        return blank(&CAST_COLS, "cast_matched");
    };
    let mut m = Map::new();
    m.insert(
        "cast_full_name".into(),
        super::get_or_empty(member, "full_name"),
    );
    m.insert(
        "cast_voice_id".into(),
        super::get_or_empty(member, "voice_id"),
    );
    m.insert("cast_pan".into(), super::get_or_empty(member, "pan"));
    m.insert(
        "cast_filter".into(),
        Value::String(
            if member.get("filter").is_some_and(super::truthy) {
                "TRUE"
            } else {
                "FALSE"
            }
            .into(),
        ),
    );
    m.insert("cast_role".into(), super::get_or_empty(member, "role"));
    m.insert("cast_matched".into(), Value::String("TRUE".into()));
    m
}

/// `(total_rows, direction_rows, sfx_matched, dialogue_rows, cast_matched)`.
pub fn annotate_csv(
    csv_path: &Path,
    sfx_path: &Path,
    cast_path: &Path,
    out_path: &Path,
) -> anyhow::Result<(usize, usize, usize, usize, usize)> {
    let sfx_cfg: Value = serde_json::from_str(&fs::read_to_string(sfx_path)?)?;
    let effects = sfx_cfg
        .get("effects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let default_influence = sfx_cfg
        .get("defaults")
        .and_then(|d| d.get("prompt_influence"))
        .cloned()
        .unwrap_or(Value::Null);

    let cast_cfg: Value = serde_json::from_str(&fs::read_to_string(cast_path)?)?;
    let cast = cast_cfg
        .get("cast")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let rows = pycsv::read_dicts(&fs::read_to_string(csv_path)?);

    let direction_rows = rows
        .iter()
        .filter(|r| row_get(r, "type") == "direction")
        .count();
    let dialogue_rows = rows
        .iter()
        .filter(|r| row_get(r, "type") == "dialogue")
        .count();
    let sfx_matched = rows
        .iter()
        .filter(|r| row_get(r, "type") == "direction" && effects.contains_key(row_get(r, "text")))
        .count();
    let cast_matched = rows
        .iter()
        .filter(|r| row_get(r, "type") == "dialogue" && cast.contains_key(row_get(r, "speaker")))
        .count();

    let cols: Vec<&str> = INPUT_COLS
        .iter()
        .chain(SFX_COLS.iter())
        .chain(CAST_COLS.iter())
        .copied()
        .collect();
    let out_rows: Vec<Map<String, Value>> = rows
        .iter()
        .map(|row| {
            let mut m = Map::new();
            for c in INPUT_COLS {
                m.insert(c.into(), Value::String(row_get(row, c).to_string()));
            }
            m.extend(join_sfx(row, &effects, &default_influence));
            m.extend(join_cast(row, &cast));
            m
        })
        .collect();
    let mut f = fs::File::create(out_path)?;
    pycsv::write_dicts(&mut f, &cols, &out_rows)?;

    Ok((
        rows.len(),
        direction_rows,
        sfx_matched,
        dialogue_rows,
        cast_matched,
    ))
}

fn execute(a: &Args) -> anyhow::Result<i32> {
    let tag = a
        .episode
        .clone()
        .or_else(|| a.tag.clone())
        .unwrap_or_default();
    let slug = resolve_slug(a.show.as_deref(), "project.json");
    let p = derive_paths(&slug, &tag);
    let csv_path = a
        .csv_path
        .clone()
        .unwrap_or_else(|| p["parsed_csv"].clone());
    let sfx_path = a.sfx_path.clone().unwrap_or_else(|| p["sfx"].clone());
    let cast_path = a.cast_path.clone().unwrap_or_else(|| p["cast"].clone());
    let out_path = a
        .out_path
        .clone()
        .unwrap_or_else(|| p["annotated_csv"].clone());

    if abspath(&out_path) == abspath(&csv_path) {
        log::error(&format!(
            "output path '{}' is the same as input '{}'",
            out_path.display(),
            csv_path.display()
        ));
        return Ok(1);
    }
    for (path, label) in [
        (&csv_path, "CSV"),
        (&sfx_path, "SFX JSON"),
        (&cast_path, "Cast JSON"),
    ] {
        if !path.exists() {
            log::error(&format!("{label} file not found: {}", path.display()));
            return Ok(1);
        }
    }

    let (total, n_dir, sfx_hit, n_dlg, cast_hit) =
        annotate_csv(&csv_path, &sfx_path, &cast_path, &out_path)?;
    log::info(&format!("Rows written: {total}"));
    log::info(&format!(
        "  SFX matched:  {sfx_hit} / {n_dir} direction rows"
    ));
    log::info(&format!(
        "  Cast matched: {cast_hit} / {n_dlg} dialogue rows"
    ));
    log::info(&format!("Output: {}", out_path.display()));
    Ok(0)
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("csv-join");
    // Python parses inside the banner, so a usage error is framed too.
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-csv-join", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    execute(&a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn direction_rows_join_effects_and_inherit_default_influence() {
        let effects = json!({"SFX: DOOR": {"prompt": null, "duration_seconds": 5.0, "loop": true}});
        let m = join_sfx(
            &row(&[("type", "direction"), ("text", "SFX: DOOR")]),
            effects.as_object().unwrap(),
            &json!(0.3),
        );
        assert_eq!(m["sfx_type"], "sfx");
        assert_eq!(m["sfx_prompt"], "");
        assert_eq!(pycsv::cell(&m["sfx_duration_seconds"]), "5.0");
        assert_eq!(pycsv::cell(&m["sfx_prompt_influence"]), "0.3");
        assert_eq!(m["sfx_loop"], "TRUE");
        assert_eq!(m["sfx_slug"], "sfx_door");
        assert_eq!(m["sfx_matched"], "TRUE");
    }

    #[test]
    fn other_rows_and_misses_are_blank() {
        let effects = json!({}).as_object().unwrap().clone();
        let m = join_sfx(&row(&[("type", "dialogue")]), &effects, &Value::Null);
        assert_eq!(m["sfx_matched"], "FALSE");
        assert_eq!(m["sfx_type"], "");
        // A short CSV row has no "text" column at all.
        let m = join_sfx(&row(&[("type", "direction")]), &effects, &Value::Null);
        assert_eq!(m["sfx_matched"], "FALSE");
    }

    #[test]
    fn cast_filter_is_truthiness_not_presence() {
        let cast = json!({"host": {"full_name": "Ada", "filter": "", "pan": -0.5},
                          "guest": {"filter": "phone"}});
        let cast = cast.as_object().unwrap();
        let m = join_cast(&row(&[("type", "dialogue"), ("speaker", "host")]), cast);
        assert_eq!(m["cast_filter"], "FALSE");
        assert_eq!(pycsv::cell(&m["cast_pan"]), "-0.5");
        assert_eq!(m["cast_role"], "");
        let m = join_cast(&row(&[("type", "dialogue"), ("speaker", "guest")]), cast);
        assert_eq!(m["cast_filter"], "TRUE");
        let m = join_cast(&row(&[("type", "dialogue"), ("speaker", "")]), cast);
        assert_eq!(m["cast_matched"], "FALSE");
    }
}
