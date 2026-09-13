//! Multitrack timeline views. Port of `timeline_viz.py`.
//!
//! The HTML page's ~900 lines of CSS and JavaScript live in
//! `templates/timeline.html.tmpl`, rendered out of the reference Python by
//! `tools/templates/extract.py` with a marker for each dynamic value.

use std::fs;
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Map, Value};
use xil_core::fsutil::{abspath, relpath};
use xil_core::pyfmt::html_escape;
use xil_core::pyjson::{dumps, py_float, Style};

use super::Label;

const HTML_TEMPLATE: &str = include_str!("templates/timeline.html.tmpl");

/// `TimelineData`: the five layers, plus section and scene bands.
pub struct TimelineData {
    pub tag: String,
    pub total_duration_s: f64,
    pub layers: IndexMap<&'static str, Vec<Label>>,
    pub sections: Vec<Label>,
    pub scenes: Vec<Label>,
}

/// `build_timeline_data(...)`.
#[allow(clippy::too_many_arguments)]
pub fn build_timeline_data(
    tag: &str,
    total_s: f64,
    dlg: Vec<Label>,
    amb: Vec<Label>,
    mus: Vec<Label>,
    sfx: Vec<Label>,
    vf: Vec<Label>,
    sections: Vec<Label>,
    scenes: Vec<Label>,
) -> TimelineData {
    let mut layers = IndexMap::new();
    layers.insert("dialogue", dlg);
    layers.insert("ambience", amb);
    layers.insert("music", mus);
    layers.insert("sfx", sfx);
    layers.insert("vintage_filter", vf);
    TimelineData {
        tag: tag.to_string(),
        total_duration_s: total_s,
        layers,
        sections,
        scenes,
    }
}

/// `_format_time(seconds)` — `M:SS`.
pub fn format_time(seconds: f64) -> String {
    let whole = seconds as i64;
    format!("{}:{:02}", whole.div_euclid(60), whole.rem_euclid(60))
}

/// `shutil.get_terminal_size((120, 24)).columns`.
fn terminal_columns() -> usize {
    let env_cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    if env_cols > 0 {
        return env_cols as usize;
    }
    match stdout_columns() {
        Some(c) if c > 0 => c,
        _ => 120,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn stdout_columns() -> Option<usize> {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }
    extern "C" {
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
    }
    #[cfg(target_os = "linux")]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x5413;
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;
    let mut ws = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ fills a `struct winsize`; fd 1 is stdout.
    let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut ws as *mut Winsize) };
    (rc == 0).then_some(ws.ws_col as usize)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn stdout_columns() -> Option<usize> {
    None
}

/// `render_terminal_timeline(data)`.
pub fn render_terminal_timeline(data: &TimelineData) -> String {
    render_terminal_timeline_width(data, terminal_columns())
}

pub fn render_terminal_timeline_width(data: &TimelineData, width: usize) -> String {
    let total_s = data.total_duration_s;
    if total_s <= 0.0 {
        return format!("--- Timeline: {} (0:00) ---\n  (no audio)\n", data.tag);
    }
    let label_col = 12usize;
    let track_width = (width as i64 - label_col as i64 - 2).max(20) as usize;
    let interval = if total_s <= 180.0 {
        30
    } else if total_s <= 600.0 {
        60
    } else {
        120
    };

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "--- Timeline: {} ({}) ---",
        data.tag,
        format_time(total_s)
    ));
    lines.push(String::new());

    let num_ticks = (total_s / interval as f64).floor() as i64 + 1;
    let col_of = |t: f64| (t / total_s * track_width as f64) as i64;

    let mut ruler: Vec<char> = " ".repeat(label_col).chars().collect();
    for i in 0..num_ticks {
        let t = (i * interval) as f64;
        let col = col_of(t);
        if col >= track_width as i64 {
            break;
        }
        let pad = col - (ruler.len() as i64 - label_col as i64);
        if pad > 0 {
            ruler.extend(std::iter::repeat(' ').take(pad as usize));
        }
        ruler.extend(format_time(t).chars());
    }

    let mut ticks = vec![' '; track_width];
    for i in 0..num_ticks {
        let t = (i * interval) as f64;
        let col = col_of(t);
        if col >= track_width as i64 {
            break;
        }
        let col = col as usize;
        ticks[col] = if i == 0 {
            '├'
        } else if col == track_width - 1 {
            '┤'
        } else {
            '┼'
        };
    }
    for c in ticks.iter_mut() {
        if *c == ' ' {
            *c = '─';
        }
    }
    lines.push(ruler.into_iter().collect());
    lines.push(format!(
        "{}{}",
        " ".repeat(label_col),
        ticks.iter().collect::<String>()
    ));
    lines.push(String::new());

    let config = [
        ("dialogue", "DIALOGUE", '█'),
        ("ambience", "AMBIENCE", '▓'),
        ("music", "MUSIC", '█'),
        ("sfx", "SFX", '█'),
        ("vintage_filter", "VTG FILTER", '▒'),
    ];
    for (key, name, fill) in config {
        let spans = data.layers.get(key).map(Vec::as_slice).unwrap_or(&[]);
        if spans.is_empty() {
            continue;
        }
        let mut bar = vec![' '; track_width];
        let mut positions: Vec<(usize, Vec<char>)> = Vec::new();
        for span in spans {
            let col_start = (span.start_s / total_s * track_width as f64) as i64;
            let col_end = (span.end_s / total_s * track_width as f64) as i64;
            let col_start = col_start.min(track_width as i64 - 1).max(0);
            let col_end = col_end.min(track_width as i64).max(col_start + 1);
            let ch = if col_end - col_start <= 1 && key == "sfx" {
                if span.end_s - span.start_s < 1.5 {
                    '·'
                } else {
                    fill
                }
            } else {
                fill
            };
            for c in col_start..col_end {
                bar[c as usize] = ch;
            }
            let mut label: Vec<char> = span.text.chars().collect();
            if label.len() > 12 {
                label.truncate(11);
                label.push('…');
            }
            positions.push((col_start as usize, label));
        }
        let mut label_row = vec![' '; track_width];
        for (col, lbl) in positions {
            let end = (col + lbl.len()).min(track_width);
            if (col..end).all(|i| label_row[i] == ' ') {
                for (i, ch) in lbl.iter().enumerate() {
                    if col + i < track_width {
                        label_row[col + i] = *ch;
                    }
                }
            }
        }
        let padded = format!("  {:<width$}", name, width = label_col - 2);
        lines.push(format!("{padded}{}", bar.iter().collect::<String>()));
        lines.push(format!(
            "{}{}",
            " ".repeat(label_col),
            label_row.iter().collect::<String>()
        ));
        lines.push(String::new());
    }
    lines.join("\n")
}

/// `_fmt_mmss_tenths(seconds)` — `M:SS.t`, right-aligned to 7.
fn fmt_mmss_tenths(seconds: f64) -> String {
    let m = (seconds as i64).div_euclid(60);
    let s = seconds - (m * 60) as f64;
    let text = format!("{m}:{s:04.1}");
    format!("{text:>7}")
}

/// `render_text_timeline_map(data, output_path, slug=...)`.
pub fn render_text_timeline_map(
    data: &TimelineData,
    output_path: &Path,
    slug: &str,
) -> std::io::Result<()> {
    let empty = Vec::new();
    let dialogue = data.layers.get("dialogue").unwrap_or(&empty);
    let sfx = data.layers.get("sfx").unwrap_or(&empty);
    let mut spans: Vec<(bool, &Label)> = dialogue
        .iter()
        .map(|l| (true, l))
        .chain(sfx.iter().map(|l| (false, l)))
        .collect();
    spans.sort_by(|a, b| {
        a.1.start_s
            .partial_cmp(&b.1.start_s)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.seq.unwrap_or(0).cmp(&b.1.seq.unwrap_or(0)))
    });
    let show_part = if slug.is_empty() {
        String::new()
    } else {
        format!(" — {slug}")
    };
    let mut lines = vec![
        format!(
            "# Timeline map: {}{show_part} ({})",
            data.tag,
            format_time(data.total_duration_s)
        ),
        "# dialogue + SFX foreground timing; music/ambience omitted".to_string(),
        "#".to_string(),
        "#  START      END       LAYER  SEQ   WHO/WHAT".to_string(),
    ];
    for (is_dlg, sp) in spans {
        let layer = if is_dlg { "DLG" } else { "SFX" };
        let seq = match sp.seq {
            Some(s) => format!("#{s:03}"),
            None => "    ".to_string(),
        };
        let who = match (&sp.snippet, is_dlg) {
            (Some(snip), true) if !snip.is_empty() => format!("{}  “{snip}…”", sp.text),
            _ => sp.text.clone(),
        };
        lines.push(format!(
            " {} – {}   {layer}   {seq}  {who}",
            fmt_mmss_tenths(sp.start_s),
            fmt_mmss_tenths(sp.end_s)
        ));
    }
    if let Some(parent) = output_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, lines.join("\n") + "\n")
}

fn opt_json<T>(v: &Option<T>, f: impl Fn(&T) -> Value) -> Value {
    v.as_ref().map(f).unwrap_or(Value::Null)
}

fn span_json(sp: &Label) -> Value {
    let mut m = Map::new();
    m.insert("start_s".into(), py_float(sp.start_s));
    m.insert("end_s".into(), py_float(sp.end_s));
    m.insert("label".into(), Value::String(sp.text.clone()));
    m.insert("ramp_in_s".into(), opt_json(&sp.ramp_in_s, |n| n.to_json()));
    m.insert(
        "ramp_out_s".into(),
        opt_json(&sp.ramp_out_s, |n| n.to_json()),
    );
    m.insert(
        "play_duration".into(),
        opt_json(&sp.play_duration, |x| py_float(*x)),
    );
    m.insert(
        "snippet".into(),
        opt_json(&sp.snippet, |s| Value::String(s.clone())),
    );
    m.insert(
        "volume_pct".into(),
        opt_json(&sp.volume_pct, |n| n.to_json()),
    );
    m.insert("seq".into(), opt_json(&sp.seq, |s| Value::from(*s)));
    m.insert(
        "tts_model".into(),
        opt_json(&sp.tts_model, |s| Value::String(s.clone())),
    );
    Value::Object(m)
}

fn band_json(sp: &Label) -> Value {
    let mut m = Map::new();
    m.insert("start_s".into(), py_float(sp.start_s));
    m.insert("end_s".into(), py_float(sp.end_s));
    m.insert("label".into(), Value::String(sp.text.clone()));
    Value::Object(m)
}

/// `os.path.relpath(full, out_dir)` with `/` separators and an mtime
/// cache-buster.
fn rel_audio(full_abs: &Path, out_dir: &Path) -> String {
    let rel = relpath(full_abs, out_dir)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let mtime = fs::metadata(full_abs)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{rel}?v={mtime}")
}

/// `render_html_timeline(data, output_path, stems_dir, slug=, tag=, layers_dir=)`.
pub fn render_html_timeline(
    data: &TimelineData,
    output_path: &Path,
    stems_dir: Option<&Path>,
    slug: &str,
    tag: &str,
    layers_dir: Option<&Path>,
) -> std::io::Result<()> {
    let out_dir = abspath(output_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();

    let seq_re = regex::Regex::new(r"^(n?)(\d+)_").expect("static regex");
    let mut clips = Map::new();
    if let Some(dir) = stems_dir.filter(|d| d.is_dir()) {
        let mut names: Vec<String> = fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for fname in names {
            if !fname.ends_with(".mp3") {
                continue;
            }
            if let Some(m) = seq_re.captures(&fname) {
                let Ok(n) = m[2].parse::<i64>() else { continue };
                let seq = if &m[1] == "n" { -n } else { n };
                let full = abspath(&dir.join(&fname));
                clips.insert(seq.to_string(), Value::String(rel_audio(&full, &out_dir)));
            }
        }
    }
    let clips_json = dumps(&Value::Object(clips), Style::COMPACT);

    let mut layers = Map::new();
    for (key, spans) in &data.layers {
        layers.insert(
            key.to_string(),
            Value::Array(spans.iter().map(span_json).collect()),
        );
    }
    let mut json_data = Map::new();
    json_data.insert("tag".into(), Value::String(data.tag.clone()));
    json_data.insert("total_duration_s".into(), py_float(data.total_duration_s));
    json_data.insert("layers".into(), Value::Object(layers));
    json_data.insert(
        "sections".into(),
        Value::Array(data.sections.iter().map(band_json).collect()),
    );
    json_data.insert(
        "scenes".into(),
        Value::Array(data.scenes.iter().map(band_json).collect()),
    );

    let span_count: usize = data.layers.values().map(Vec::len).sum();
    let slug_or_tag = if slug.is_empty() { &data.tag } else { slug };
    let tag_or_tag = if tag.is_empty() { &data.tag } else { tag };
    let slug_js = format!(
        "const XIL_SLUG = {};\nconst XIL_TAG  = {};",
        dumps(&Value::String(slug_or_tag.to_string()), Style::COMPACT),
        dumps(&Value::String(tag_or_tag.to_string()), Style::COMPACT)
    );

    let mut layer_audio = Map::new();
    if let Some(dir) = layers_dir.filter(|d| d.is_dir()) {
        for key in ["dialogue", "sfx", "music", "ambience", "vintage_filter"] {
            let wav = dir.join(format!("{}_layer_{key}.wav", data.tag));
            if wav.exists() {
                layer_audio.insert(
                    key.into(),
                    Value::String(rel_audio(&abspath(&wav), &out_dir)),
                );
            }
        }
    }

    let content = HTML_TEMPLATE
        .replace("@@XIL_TAG_ESCAPED@@", &html_escape(&data.tag))
        .replace("@@XIL_DURATION@@", &format_time(data.total_duration_s))
        .replace("@@XIL_SPAN_COUNT@@", &span_count.to_string())
        .replace("@@XIL_CLIPS_JSON@@", &clips_json)
        .replace(
            "@@XIL_GENERATED_AT@@",
            &chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        )
        .replace("@@XIL_SLUG_JS@@", &slug_js)
        .replace(
            "@@XIL_LAYER_AUDIO_JSON@@",
            &dumps(&Value::Object(layer_audio), Style::COMPACT),
        )
        // Last: the data JSON is the only value that could contain a marker.
        .replace(
            "@@XIL_DATA_JSON@@",
            &dumps(&Value::Object(json_data), Style::COMPACT),
        );

    if let Some(parent) = output_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formats() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(125.9), "2:05");
        assert_eq!(fmt_mmss_tenths(5.25), " 0:05.2");
        assert_eq!(fmt_mmss_tenths(65.35), " 1:05.3");
    }

    #[test]
    fn empty_timeline_says_so() {
        let td = build_timeline_data(
            "S01E01",
            0.0,
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            render_terminal_timeline_width(&td, 80),
            "--- Timeline: S01E01 (0:00) ---\n  (no audio)\n"
        );
    }

    #[test]
    fn template_carries_every_marker() {
        for m in [
            "@@XIL_TAG_ESCAPED@@",
            "@@XIL_DURATION@@",
            "@@XIL_SPAN_COUNT@@",
            "@@XIL_DATA_JSON@@",
            "@@XIL_CLIPS_JSON@@",
            "@@XIL_GENERATED_AT@@",
            "@@XIL_SLUG_JS@@",
            "@@XIL_LAYER_AUDIO_JSON@@",
        ] {
            assert!(HTML_TEMPLATE.contains(m), "{m}");
        }
    }
}
