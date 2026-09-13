//! `xil voices` — list the workspace's ElevenLabs voices with enriched
//! metadata, or back-fill a cast file from them. Port of
//! `XILU001_discover_voices_T2S.py`.

use std::ffi::OsString;
use std::fs;

use crate::cmd::py_str;
use clap::Parser;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use xil_audio::fx::py_repr;
use xil_core::pyfmt::head;
use xil_core::pyjson::{dumps, Style};
use xil_core::{banner, log};

#[derive(Parser)]
#[command(
    name = "xil-voices",
    about = "List ElevenLabs voices with enriched metadata"
)]
struct Args {
    /// Filter by category: premade cloned generated professional
    #[arg(long, num_args = 1.., value_name = "CAT")]
    category: Option<Vec<String>>,
    /// Case-insensitive substring filter on name or description
    #[arg(long, value_name = "TEXT")]
    search: Option<String>,
    /// Show full detail for a single voice ID
    #[arg(long, value_name = "VOICE_ID")]
    id: Option<String>,
    /// Print all fields for each voice
    #[arg(long, short = 'v')]
    verbose: bool,
    /// Output results as JSON array
    #[arg(long)]
    json: bool,
    /// Back-fill role and language_code in a cast JSON from API voice metadata
    #[arg(long, value_name = "CAST_JSON")]
    update_cast: Option<String>,
    /// With --update-cast: show changes without writing the file
    #[arg(long)]
    dry_run: bool,
}

fn category_label(cat: &str) -> Option<&'static str> {
    match cat {
        "premade" => Some("Premade"),
        "cloned" => Some("Instant Clone"),
        "generated" => Some("Generated"),
        "professional" => Some("Professional Clone (PVC)"),
        _ => None,
    }
}

fn sharing_label(cat: &str) -> Option<&'static str> {
    match cat {
        "professional" => Some("Professional Clone"),
        "high_quality" => Some("High Quality"),
        _ => None,
    }
}

fn truthy(v: &Value) -> bool {
    crate::cmd::truthy(v)
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// `_fmt_unix(ts)` — `YYYY-MM-DD` in UTC.
fn fmt_unix(ts: &Value) -> String {
    ts.as_i64()
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// `build_voice_record(v)`, keys in the Python dict's order.
fn build_voice_record(v: &Value) -> Map<String, Value> {
    let get = |k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    let labels = v
        .get("labels")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let sharing = v.get("sharing").filter(|x| x.is_object());
    let sfield = |k: &str| {
        sharing
            .and_then(|sh| sh.get(k))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let sharing_cat = match sharing {
        Some(_) => {
            let c = sfield("category");
            match c.as_str().and_then(sharing_label) {
                Some(l) => Value::from(l),
                None => c,
            }
        }
        None => Value::Null,
    };
    let description = [get("description"), sfield("description")]
        .into_iter()
        .find(truthy)
        .unwrap_or_else(|| Value::from(""));
    let langs_all: Vec<String> = v
        .get("verified_languages")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|l| {
                    l.get("language")
                        .map(py_str)
                        .unwrap_or_else(|| "None".into())
                })
                .collect()
        })
        .unwrap_or_default();
    let mut langs: Vec<String> = Vec::new();
    for l in &langs_all {
        if !langs.contains(l) {
            langs.push(l.clone());
        }
    }
    let lang_str = if langs.is_empty() {
        String::new()
    } else if langs.len() <= 3 {
        langs.join(", ")
    } else {
        format!("{} (+{})", langs[..3].join(", "), langs.len() - 3)
    };
    let label = |k: &str| labels.get(k).cloned().unwrap_or_else(|| Value::from(""));

    let mut r = Map::new();
    r.insert("voice_id".into(), get("voice_id"));
    r.insert("name".into(), get("name"));
    r.insert(
        "library_name".into(),
        if sharing.is_some() {
            sfield("name")
        } else {
            Value::Null
        },
    );
    r.insert("category".into(), get("category"));
    r.insert("sharing_category".into(), sharing_cat);
    r.insert("description".into(), description);
    for k in [
        "gender",
        "age",
        "accent",
        "descriptive",
        "use_case",
        "language",
    ] {
        r.insert(k.into(), label(k));
    }
    r.insert("verified_languages".into(), lang_str.into());
    r.insert("verified_lang_count".into(), langs.len().into());
    let hq = v
        .get("high_quality_base_model_ids")
        .filter(|x| truthy(x))
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]));
    r.insert("high_quality_models".into(), hq);
    r.insert("is_owner".into(), get("is_owner"));
    r.insert("is_bookmarked".into(), get("is_bookmarked"));
    r.insert("permission".into(), get("permission_on_resource"));
    r.insert(
        "created_at".into(),
        fmt_unix(&get("created_at_unix")).into(),
    );
    r.insert(
        "notice_days".into(),
        if sharing.is_some() {
            sfield("notice_period")
        } else {
            Value::Null
        },
    );
    r
}

fn or_dash(v: &Value) -> String {
    if truthy(v) {
        py_str(v)
    } else {
        "—".into()
    }
}

fn cat_label(rec: &Map<String, Value>) -> String {
    let cat = &rec["category"];
    match cat.as_str().and_then(category_label) {
        Some(l) => l.to_string(),
        None if truthy(cat) => py_str(cat),
        None => "?".into(),
    }
}

fn print_verbose(rec: &Map<String, Value>) {
    let mut label = cat_label(rec);
    if truthy(&rec["sharing_category"]) {
        label = format!("{label} / {}", py_str(&rec["sharing_category"]));
    }
    log::info(&format!("  Name         : {}", py_str(&rec["name"])));
    if truthy(&rec["library_name"]) && rec["library_name"] != rec["name"] {
        log::info(&format!(
            "  Library name : {}",
            py_str(&rec["library_name"])
        ));
    }
    log::info(&format!("  Voice ID     : {}", py_str(&rec["voice_id"])));
    log::info(&format!("  Category     : {label}"));
    if truthy(&rec["description"]) {
        log::info(&format!("  Description  : {}", py_str(&rec["description"])));
    }
    log::info(&format!("  Gender       : {}", or_dash(&rec["gender"])));
    log::info(&format!("  Age          : {}", or_dash(&rec["age"])));
    log::info(&format!("  Accent       : {}", or_dash(&rec["accent"])));
    log::info(&format!(
        "  Tone/style   : {}",
        or_dash(&rec["descriptive"])
    ));
    log::info(&format!("  Use case     : {}", or_dash(&rec["use_case"])));
    log::info(&format!("  Language     : {}", or_dash(&rec["language"])));
    if truthy(&rec["verified_languages"]) {
        log::info(&format!(
            "  Verified langs: {} ({} total)",
            py_str(&rec["verified_languages"]),
            py_str(&rec["verified_lang_count"])
        ));
    }
    if truthy(&rec["high_quality_models"]) {
        let models: Vec<String> = rec["high_quality_models"]
            .as_array()
            .into_iter()
            .flatten()
            .map(py_str)
            .collect();
        log::info(&format!("  HQ models    : {}", models.join(", ")));
    }
    log::info(&format!(
        "  Owner        : {}",
        if truthy(&rec["is_owner"]) {
            "Yes"
        } else {
            "No (library copy)"
        }
    ));
    log::info(&format!(
        "  Bookmarked   : {}",
        if truthy(&rec["is_bookmarked"]) {
            "Yes"
        } else {
            "No"
        }
    ));
    log::info(&format!(
        "  Permission   : {}",
        if truthy(&rec["permission"]) {
            py_str(&rec["permission"])
        } else {
            "none".into()
        }
    ));
    log::info(&format!("  Created      : {}", or_dash(&rec["created_at"])));
    if truthy(&rec["notice_days"]) {
        log::info(&format!(
            "  Notice period: {} days",
            py_str(&rec["notice_days"])
        ));
    }
    log::info("");
}

fn or_q(v: &Value) -> String {
    if truthy(v) {
        py_str(v)
    } else {
        "?".into()
    }
}

fn print_compact(rec: &Map<String, Value>) {
    let langs = if truthy(&rec["verified_languages"]) {
        format!(" | langs: {}", py_str(&rec["verified_languages"]))
    } else {
        String::new()
    };
    let desc = if truthy(&rec["description"]) {
        format!(" | {}", head(s(&rec["description"]), 60))
    } else {
        String::new()
    };
    let name = xil_core::pyfmt::pad_right(&py_str(&rec["name"]), 28);
    log::info(&format!(
        "  {name} {}  [{}]\n    {}, {}, {}, {}{langs}{desc}",
        py_str(&rec["voice_id"]),
        cat_label(rec),
        or_q(&rec["gender"]),
        or_q(&rec["age"]),
        or_q(&rec["accent"]),
        or_q(&rec["descriptive"]),
    ));
}

fn update_cast(
    path: &str,
    by_id: &IndexMap<String, Map<String, Value>>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mut cast: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    let mut changes: Vec<String> = Vec::new();
    if let Some(members) = cast.get_mut("cast").and_then(Value::as_object_mut) {
        for (key, member) in members.iter_mut() {
            let Some(m) = member.as_object_mut() else {
                continue;
            };
            let vid = m
                .get("voice_id")
                .cloned()
                .unwrap_or_else(|| Value::from("TBD"));
            if vid == "TBD" {
                log::info(&format!("  {key}: voice_id is TBD — skipping"));
                continue;
            }
            let Some(rec) = vid.as_str().and_then(|v| by_id.get(v)) else {
                log::info(&format!(
                    "  {key} ({}): not found in workspace voices — skipping",
                    py_str(&vid)
                ));
                continue;
            };
            if m.get("role") == Some(&Value::from("TBD"))
                && rec.get("description").is_some_and(truthy)
            {
                let old = py_str(&m["role"]);
                m.insert("role".into(), rec["description"].clone());
                changes.push(format!(
                    "  {key}.role: {} → {}",
                    py_repr(&old),
                    py_repr(&py_str(&m["role"]))
                ));
            }
            if !m.get("language_code").is_some_and(truthy)
                && rec.get("language").is_some_and(truthy)
            {
                m.insert("language_code".into(), rec["language"].clone());
                changes.push(format!(
                    "  {key}.language_code: null → {}",
                    py_repr(&py_str(&m["language_code"]))
                ));
            }
        }
    }
    if changes.is_empty() {
        log::info("  No updates needed — cast file is already fully populated.");
        return Ok(());
    }
    log::info(&format!(
        "  {}Changes ({}):",
        if dry_run { "(dry run) " } else { "" },
        changes.len()
    ));
    for c in &changes {
        log::info(c);
    }
    if !dry_run {
        fs::write(path, dumps(&cast, Style::INDENT2_UTF8) + "\n")?;
        log::info(&format!("  Written: {path}"));
    }
    Ok(())
}

pub fn run(args: &[OsString]) -> anyhow::Result<i32> {
    log::init("voices");
    let _banner = banner::begin(super::prog(), &super::argv_line(args));
    let a: Args = match super::parse_or_exit("xil-voices", args) {
        Ok(a) => a,
        Err(code) => return Ok(code),
    };
    let client = xil_api::elevenlabs::Client::from_env();
    let response = client
        .voices_get_all()
        .map_err(|e| anyhow::anyhow!("elevenlabs.core.api_error.ApiError: {e}"))?;
    let mut records: Vec<Map<String, Value>> = response["voices"]
        .as_array()
        .into_iter()
        .flatten()
        .map(build_voice_record)
        .collect();
    let by_id: IndexMap<String, Map<String, Value>> = records
        .iter()
        .map(|r| (py_str(&r["voice_id"]), r.clone()))
        .collect();

    if let Some(cast) = a.update_cast.as_ref().filter(|c| !c.is_empty()) {
        log::info(&format!("\n--- Updating cast file: {cast} ---"));
        update_cast(cast, &by_id, a.dry_run)?;
        return Ok(0);
    }
    if let Some(id) = a.id.as_ref().filter(|i| !i.is_empty()) {
        match records.iter().find(|r| r["voice_id"].as_str() == Some(id)) {
            Some(r) => print_verbose(r),
            None => log::info(&format!(
                "Voice ID {} not found in your workspace.",
                py_repr(id)
            )),
        }
        return Ok(0);
    }
    if let Some(cats) = a.category.as_ref().filter(|c| !c.is_empty()) {
        let cats: Vec<String> = cats.iter().map(|c| c.to_lowercase()).collect();
        records.retain(|r| cats.contains(&s(&r["category"]).to_lowercase()));
    }
    if let Some(q) = a.search.as_ref().filter(|q| !q.is_empty()) {
        let q = q.to_lowercase();
        records.retain(|r| {
            ["name", "description", "library_name"]
                .iter()
                .any(|k| s(&r[*k]).to_lowercase().contains(&q))
        });
    }
    records.sort_by(|x, y| {
        let kx = (!truthy(&x["is_bookmarked"]), s(&x["name"]).to_lowercase());
        let ky = (!truthy(&y["is_bookmarked"]), s(&y["name"]).to_lowercase());
        kx.cmp(&ky)
    });

    if a.json {
        let arr = Value::Array(records.into_iter().map(Value::Object).collect());
        println!("{}", dumps(&arr, Style::INDENT2));
        return Ok(0);
    }

    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for r in &records {
        let c = if truthy(&r["category"]) {
            py_str(&r["category"])
        } else {
            "unknown".into()
        };
        *counts.entry(c).or_default() += 1;
    }
    log::info(&format!(
        "\n--- ElevenLabs Voices ({} shown) ---",
        records.len()
    ));
    for (cat, n) in &counts {
        log::info(&format!(
            "  {}: {n}",
            category_label(cat)
                .map(str::to_string)
                .unwrap_or_else(|| cat.clone())
        ));
    }
    log::info("");
    if a.verbose {
        records.iter().for_each(print_verbose);
    } else {
        records.iter().for_each(print_compact);
        log::info("");
        log::info("  Use --verbose for full details, --json for machine-readable output,");
        log::info("  --id <VOICE_ID> for a single voice, --category / --search to filter.");
    }
    Ok(0)
}
