//! ffmpeg-backed dialogue treatments. Port of `audio_fx.py`.
//!
//! Audio goes through ffmpeg as raw PCM over pipes, exactly as the Python
//! does: same filter graphs, same argument lists, same trim-or-pad back to
//! the input length. For a deterministic graph that makes the output bytes
//! identical; `film` draws its grain from `anoisesrc` with ffmpeg's default
//! random seed, so no two renders of it — in either language — match.
//!
//! Failures degrade: a treatment that cannot run leaves the stem as it was
//! and warns once per (treatment, reason). Setting `XIL_STRICT_FX` turns
//! that into an error instead.

use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use indexmap::IndexMap;
use sha2::{Digest, Sha256};
use xil_core::log;

use crate::segment::Segment;

pub const STRICT_ENV_VAR: &str = "XIL_STRICT_FX";

/// FIFO-bounded result cache, keyed on the input bytes and every parameter.
const CACHE_MAX_ENTRIES: usize = 512;

/// A named treatment, as written in a cast config `filter` field.
pub struct Treatment {
    pub name: &'static str,
    /// `-filter_complex` graph with `{rate}` / `{layout}` placeholders.
    pub graph: &'static str,
    pub codec: Option<&'static str>,
    pub container: Option<&'static str>,
    pub codec_rate: Option<u32>,
}

pub const FILM: Treatment = Treatment {
    name: "film",
    graph: concat!(
        "[0:a]",
        "highpass=f=90:poles=2,",
        "lowpass=f=5500:poles=2,",
        "lowshelf=f=180:g=3,",
        "equalizer=f=350:w=1.2:t=q:g=2,",
        "equalizer=f=2800:w=1.6:t=q:g=-6,",
        "highshelf=f=7000:g=-4,",
        "acompressor=threshold=-24dB:ratio=3.5:attack=12:release=280:makeup=2:knee=6,",
        "volume=12dB,",
        "asoftclip=type=tanh:param=1:oversample=4,",
        "volume=-12dB,",
        "volume=8.4dB",
        "[v];",
        "anoisesrc=color=pink:amplitude=0.018:sample_rate={rate},",
        "aformat=channel_layouts={layout}[n];",
        "[v][n]amix=inputs=2:duration=first:normalize=0[out]",
    ),
    codec: None,
    container: None,
    codec_rate: None,
};

pub const SPEAKERPHONE: Treatment = Treatment {
    name: "speakerphone",
    graph: concat!(
        "[0:a]",
        "highpass=f=350:poles=2,",
        "highpass=f=350:poles=2,",
        "lowpass=f=3400:poles=2,",
        "lowpass=f=3400:poles=2,",
        "equalizer=f=700:w=1.0:t=q:g=-4,",
        "equalizer=f=1800:w=1.1:t=q:g=5,",
        "acompressor=threshold=-24dB:ratio=6:attack=5:release=120:makeup=3:knee=2,",
        "volume=8dB,",
        "asoftclip=type=atan:param=1:oversample=4,",
        "volume=-8dB,",
        "aecho=0.9:0.9:55:0.22,",
        "volume=13.6dB",
        "[out]",
    ),
    codec: None,
    container: None,
    codec_rate: None,
};

pub const PHONE: Treatment = Treatment {
    name: "phone",
    graph: concat!(
        "[0:a]",
        "highpass=f=300:poles=2,",
        "highpass=f=300:poles=2,",
        "lowpass=f=3400:poles=2,",
        "lowpass=f=3400:poles=2,",
        "equalizer=f=500:w=1.0:t=q:g=-4,",
        "equalizer=f=1700:w=1.2:t=q:g=6,",
        "acompressor=threshold=-22dB:ratio=8:attack=3:release=90:makeup=3:knee=2,",
        "volume=6dB,",
        "asoftclip=type=atan:param=1:oversample=4,",
        "volume=-6dB,",
        "volume=10dB",
        "[out]",
    ),
    codec: Some("libgsm"),
    container: Some("gsm"),
    codec_rate: Some(8000),
};

/// `TREATMENTS`, in the order the Python dict is built.
pub const TREATMENTS: [&Treatment; 3] = [&FILM, &SPEAKERPHONE, &PHONE];

pub fn treatment(name: &str) -> Option<&'static Treatment> {
    TREATMENTS.iter().copied().find(|t| t.name == name)
}

/// `AudioFxError` — only raised under `XIL_STRICT_FX`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AudioFxError(pub String);

type CacheKey = (
    [u8; 32],
    u32,
    usize,
    usize,
    String,
    bool,
    Option<String>,
    Option<String>,
    Option<u32>,
);

struct State {
    cache: IndexMap<CacheKey, Vec<u8>>,
    warned: HashSet<(String, String)>,
    encoder_probe: std::collections::HashMap<String, bool>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let state = guard.get_or_insert_with(|| State {
        cache: IndexMap::new(),
        warned: HashSet::new(),
        encoder_probe: std::collections::HashMap::new(),
    });
    f(state)
}

/// Python `repr()` of a `str`.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `_warn_once(label, reason, message)` — the message is already formatted.
pub fn warn_once(label: &str, reason: &str, message: &str) {
    let first = with_state(|s| s.warned.insert((label.to_string(), reason.to_string())));
    if first {
        log::warning(message);
    }
}

/// `_fail`: warn once with the fallback spelled out, or error when strict.
fn fail(label: &str, reason: &str, message: &str, consequence: &str) -> Result<(), AudioFxError> {
    if std::env::var_os(STRICT_ENV_VAR).is_some_and(|v| !v.is_empty()) {
        return Err(AudioFxError(message.to_string()));
    }
    warn_once(label, reason, &format!("{message} — {consequence}"));
    Ok(())
}

const UNTREATED: &str = "leaving audio untreated";
const CODEC_FALLBACK: &str = "keeping the filtered audio without codec character";

/// ffmpeg's raw format name for a sample width.
fn raw_format(width: usize) -> Option<&'static str> {
    match width {
        1 => Some("u8"),
        2 => Some("s16le"),
        3 => Some("s24le"),
        4 => Some("s32le"),
        _ => None,
    }
}

/// Run a command with `input` on stdin, collecting stdout and stderr, as
/// `subprocess.run(..., input=..., capture_output=True)` does.
fn run_piped(args: &[String], input: &[u8]) -> std::io::Result<std::process::Output> {
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = input.to_vec();
    // Write from another thread so a full stdout pipe cannot deadlock us.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output();
    let _ = writer.join();
    out
}

fn last_stderr_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .trim()
        .lines()
        .last()
        .map(str::to_string)
        .unwrap_or_else(|| "no stderr output".to_string())
}

fn s(v: &str) -> String {
    v.to_string()
}

/// `_codec_round_trip`: encode through a lossy codec and decode straight back.
fn codec_round_trip(
    raw: Vec<u8>,
    segment: &Segment,
    raw_fmt: &str,
    label: &str,
    codec: &str,
    container: Option<&str>,
    codec_rate: Option<u32>,
) -> Result<Vec<u8>, AudioFxError> {
    let rate = segment.frame_rate.to_string();
    let channels = segment.channels.to_string();
    let mux = container.unwrap_or(codec);
    let encode: Vec<String> = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-f",
        raw_fmt,
        "-ar",
        &rate,
        "-ac",
        &channels,
        "-i",
        "pipe:0",
        "-ar",
    ]
    .iter()
    .map(|x| s(x))
    .chain([codec_rate.unwrap_or(segment.frame_rate).to_string()])
    .chain(
        ["-ac", "1", "-c:a", codec, "-f", mux, "pipe:1"]
            .iter()
            .map(|x| s(x)),
    )
    .collect();
    let decode: Vec<String> = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-f",
        mux,
        "-i",
        "pipe:0",
        "-vn",
        "-sn",
        "-dn",
        "-f",
        raw_fmt,
        "-ar",
        &rate,
        "-ac",
        &channels,
        "pipe:1",
    ]
    .iter()
    .map(|x| s(x))
    .collect();

    let stage = |name: &str, args: &[String], input: &[u8]| -> Result<Vec<u8>, (String, String)> {
        match run_piped(args, input) {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => Ok(o.stdout),
            Ok(o) => Err((name.to_string(), last_stderr_line(&o.stderr))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(("missing".into(), String::new()))
            }
            Err(e) => Err(("oserror".into(), e.to_string())),
        }
    };

    let result = stage("encode", &encode, &raw).and_then(|enc| stage("decode", &decode, &enc));
    match result {
        Ok(dec) => Ok(dec),
        Err((kind, detail)) => {
            let message = match kind.as_str() {
                "missing" => format!("ffmpeg not found for codec stage of {}", py_repr(label)),
                "oserror" => format!(
                    "codec stage failed for treatment {}: {detail}",
                    py_repr(label)
                ),
                stage_name => format!(
                    "codec {codec} unavailable for treatment {} ({stage_name}): {detail}",
                    py_repr(label)
                ),
            };
            fail(label, "codec", &message, CODEC_FALLBACK)?;
            Ok(raw)
        }
    }
}

/// `_fit_length`: trim or pad with silence to exactly `target` bytes.
fn fit_length(mut raw: Vec<u8>, target: usize, width: usize) -> Vec<u8> {
    let fill = if width == 1 { 0x80 } else { 0x00 };
    raw.resize(target, fill);
    raw
}

/// `run_ffmpeg_filter(segment, graph, label=..., codec=..., ...)` with
/// `preserve_length=True`.
pub fn run_ffmpeg_filter(
    segment: &Segment,
    graph: &str,
    label: &str,
    codec: Option<&str>,
    container: Option<&str>,
    codec_rate: Option<u32>,
) -> Result<Segment, AudioFxError> {
    let Some(raw_fmt) = raw_format(segment.sample_width) else {
        fail(
            label,
            "sample_width",
            &format!(
                "Unsupported sample width {} bytes for treatment {}",
                segment.sample_width,
                py_repr(label)
            ),
            UNTREATED,
        )?;
        return Ok(segment.clone());
    };
    let layout = if segment.channels == 1 {
        "mono"
    } else {
        "stereo"
    };
    let resolved = graph
        .replace("{rate}", &segment.frame_rate.to_string())
        .replace("{layout}", layout);

    let key: CacheKey = (
        Sha256::digest(&segment.data).into(),
        segment.frame_rate,
        segment.channels,
        segment.sample_width,
        resolved.clone(),
        true,
        codec.map(str::to_string),
        container.map(str::to_string),
        codec_rate,
    );
    if let Some(hit) = with_state(|s| s.cache.get(&key).cloned()) {
        return Ok(Segment::new(
            hit,
            segment.sample_width,
            segment.frame_rate,
            segment.channels,
        ));
    }

    let rate = segment.frame_rate.to_string();
    let channels = segment.channels.to_string();
    let cmd: Vec<String> = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-f",
        raw_fmt,
        "-ar",
        &rate,
        "-ac",
        &channels,
        "-i",
        "pipe:0",
        "-filter_complex",
        &resolved,
        "-map",
        "[out]",
        "-vn",
        "-sn",
        "-dn",
        "-f",
        raw_fmt,
        "-ar",
        &rate,
        "-ac",
        &channels,
        "pipe:1",
    ]
    .iter()
    .map(|x| s(x))
    .collect();

    let proc = match run_piped(&cmd, &segment.data) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fail(
                label,
                "missing",
                &format!("ffmpeg not found for treatment {}", py_repr(label)),
                UNTREATED,
            )?;
            return Ok(segment.clone());
        }
        Err(e) => {
            fail(
                label,
                "oserror",
                &format!("ffmpeg failed for treatment {}: {e}", py_repr(label)),
                UNTREATED,
            )?;
            return Ok(segment.clone());
        }
    };
    if !proc.status.success() || proc.stdout.is_empty() {
        let rc = proc
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| signal_code(&proc.status));
        fail(
            label,
            "returncode",
            &format!(
                "ffmpeg treatment {} failed (rc={rc}): {}",
                py_repr(label),
                last_stderr_line(&proc.stderr)
            ),
            UNTREATED,
        )?;
        return Ok(segment.clone());
    }

    let mut out = proc.stdout;
    if let Some(codec) = codec {
        out = codec_round_trip(out, segment, raw_fmt, label, codec, container, codec_rate)?;
    }
    let out = fit_length(out, segment.data.len(), segment.sample_width);

    with_state(|s| {
        if s.cache.len() >= CACHE_MAX_ENTRIES {
            s.cache.shift_remove_index(0);
        }
        s.cache.insert(key, out.clone());
    });
    Ok(Segment::new(
        out,
        segment.sample_width,
        segment.frame_rate,
        segment.channels,
    ))
}

/// Python reports a signal death as a negative return code.
fn signal_code(status: &std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("-{sig}");
        }
    }
    let _ = status;
    "None".to_string()
}

/// `apply_treatment(segment, name)`.
pub fn apply_treatment(segment: &Segment, name: &str) -> Result<Segment, AudioFxError> {
    let Some(t) = treatment(name) else {
        let mut known: Vec<&str> = TREATMENTS.iter().map(|t| t.name).collect();
        known.sort();
        warn_once(
            name,
            "unknown",
            &format!(
                "Unknown ffmpeg treatment {} — known treatments: {}",
                py_repr(name),
                known.join(", ")
            ),
        );
        return Ok(segment.clone());
    };
    run_ffmpeg_filter(segment, t.graph, t.name, t.codec, t.container, t.codec_rate)
}

/// `encoder_available(name)`: does `ffmpeg -encoders` list it as a token?
pub fn encoder_available(name: &str) -> bool {
    match Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .any(|t| t == name),
        _ => false,
    }
}

/// `missing_codecs(names)`: treatment → encoder, for encoders this ffmpeg lacks.
pub fn missing_codecs<'a>(names: impl IntoIterator<Item = &'a str>) -> IndexMap<String, String> {
    let mut uniq: Vec<&str> = names.into_iter().collect();
    uniq.sort();
    uniq.dedup();
    let mut missing = IndexMap::new();
    for name in uniq {
        let Some(t) = treatment(name) else { continue };
        let Some(codec) = t.codec else { continue };
        let available = with_state(|s| s.encoder_probe.get(codec).copied());
        let available = match available {
            Some(a) => a,
            None => {
                let a = encoder_available(codec);
                with_state(|s| s.encoder_probe.insert(codec.to_string(), a));
                a
            }
        };
        if !available {
            missing.insert(name.to_string(), codec.to_string());
        }
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr("phone"), "'phone'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a\\b"), "'a\\\\b'");
    }

    #[test]
    fn fit_length_trims_and_pads() {
        assert_eq!(fit_length(vec![1, 2, 3, 4], 2, 2), vec![1, 2]);
        assert_eq!(fit_length(vec![1, 2], 4, 2), vec![1, 2, 0, 0]);
        assert_eq!(fit_length(vec![1], 3, 1), vec![1, 0x80, 0x80]);
    }

    #[test]
    fn treatments_keep_length() {
        if !crate::ffmpeg::available() {
            return;
        }
        let data: Vec<u8> = (0..4410i32)
            .map(|i| ((i as f64 * 0.05).sin() * 8000.0) as i16)
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let seg = Segment::new(data, 2, 44100, 1);
        let out = apply_treatment(&seg, "speakerphone").unwrap();
        assert_eq!(out.data.len(), seg.data.len());
        assert_ne!(out.data, seg.data);
        let again = apply_treatment(&seg, "speakerphone").unwrap();
        assert_eq!(out, again, "second call is served from the cache");
    }

    /// `film` mixes in `anoisesrc` grain with ffmpeg's random seed, so it
    /// cannot be pinned to bytes the way the other treatments are. Check the
    /// contract instead: stereo in, stereo out, same length, audibly changed,
    /// and grain present even where the input is silent.
    #[test]
    fn film_keeps_length_and_adds_grain() {
        if !crate::ffmpeg::available() {
            return;
        }
        let mut data: Vec<u8> = (0..8820i32)
            .map(|i| ((i as f64 * 0.03).sin() * 6000.0) as i16)
            .flat_map(|v| v.to_le_bytes())
            .collect();
        data.extend(vec![0u8; 8820 * 2]);
        let seg = Segment::new(data, 2, 44100, 2);
        let out = apply_treatment(&seg, "film").unwrap();
        assert_eq!(
            (out.channels, out.frame_rate, out.data.len()),
            (2, 44100, seg.data.len())
        );
        assert_ne!(out.data, seg.data);
        let tail = &out.data[out.data.len() - 4000..];
        assert!(
            tail.iter().any(|&b| b != 0),
            "the silent tail carries grain"
        );
    }

    #[test]
    fn unknown_treatment_passes_through() {
        let seg = Segment::new(vec![1, 0, 2, 0], 2, 8000, 1);
        assert_eq!(apply_treatment(&seg, "robot").unwrap(), seg);
    }
}
