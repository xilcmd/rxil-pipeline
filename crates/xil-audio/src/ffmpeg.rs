//! Decoding and probing through ffmpeg subprocesses.
//!
//! pydub shells out to ffmpeg too, so going through the same binary is
//! what keeps the samples identical rather than merely close. Symphonia
//! would decode MP3 slightly differently and there would be no way to
//! tell a port bug from a decoder difference.

use std::path::Path;
use std::process::Command;

use crate::pcm::Pcm;

/// What ffprobe reports about a stream, before any decoding.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub channels: u16,
    pub frame_rate: u32,
    /// Container duration in seconds, when the stream declares one.
    pub duration_s: Option<f64>,
    /// Stream bit rate in bits per second, when declared.
    pub bit_rate: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("ffmpeg is not installed or not on PATH")]
    FfmpegMissing,
    /// pydub's `CouldntDecodeError`, worded exactly as it is — the whole
    /// string reaches the operator through a warning line, ffmpeg's own
    /// stderr included.
    #[error("Decoding failed. ffmpeg returned error code: {code}\n\nOutput from ffmpeg/avlib:\n\n{stderr}")]
    CouldntDecode { code: i32, stderr: String },
    #[error("{path}: {what}")]
    Malformed { path: String, what: String },
}

/// Read the first audio stream's format without decoding it.
pub fn probe(path: &Path) -> Result<Probe, AudioError> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=channels,sample_rate,duration,bit_rate",
            "-of",
            "default=noprint_wrappers=1:nokey=0",
        ])
        .arg(path)
        .output()
        .map_err(|_| AudioError::FfmpegMissing)?;
    if !out.status.success() {
        return Err(AudioError::Malformed {
            path: path.display().to_string(),
            what: "ffprobe could not read it".into(),
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |k: &str| -> Option<String> {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .map(str::to_string)
            .filter(|v| v != "N/A")
    };
    let channels = field("channels")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| AudioError::Malformed {
            path: path.display().to_string(),
            what: "no audio stream".into(),
        })?;
    let frame_rate = field("sample_rate")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| AudioError::Malformed {
            path: path.display().to_string(),
            what: "no sample rate".into(),
        })?;
    Ok(Probe {
        channels,
        frame_rate,
        duration_s: field("duration").and_then(|v| v.parse().ok()),
        bit_rate: field("bit_rate").and_then(|v| v.parse().ok()),
    })
}

/// Decode to interleaved 16-bit PCM at the file's own rate and channel
/// count — the same shape `AudioSegment.from_file` produces.
///
/// The command line is pydub's, down to the order of the arguments and
/// the conditional `-acodec`, because on failure ffmpeg's stderr is part
/// of the error the operator sees. A leading `-v quiet` would silence
/// exactly the text that has to match.
pub fn decode(path: &Path) -> Result<Pcm, AudioError> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y").arg("-i").arg(path);
    // pydub probes first and forces a codec only when the probe found a
    // stream; a file that cannot be probed gets no -acodec, and that
    // shapes the stderr it fails with.
    if probe(path).is_ok() {
        cmd.args(["-acodec", "pcm_s16le"]);
    }
    cmd.args(["-vn", "-f", "wav", "-"]);

    let out = cmd.output().map_err(|_| AudioError::FfmpegMissing)?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(AudioError::CouldntDecode {
            code: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    parse_wav(&out.stdout).ok_or_else(|| AudioError::Malformed {
        path: path.display().to_string(),
        what: "ffmpeg produced an unreadable WAV".into(),
    })
}

/// Pull format and samples out of a RIFF/WAVE buffer.
///
/// ffmpeg writes a placeholder size when the output is a pipe, so the
/// declared `data` length is not trustworthy — whatever follows the
/// chunk header is the audio.
fn parse_wav(buf: &[u8]) -> Option<Pcm> {
    if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return None;
    }
    let u16at = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
    let u32at = |i: usize| u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);

    let (mut channels, mut rate, mut bits) = (0u16, 0u32, 0u16);
    let mut pos = 12usize;
    while pos + 8 <= buf.len() {
        let id = &buf[pos..pos + 4];
        let declared = u32at(pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= buf.len() {
            channels = u16at(body + 2);
            rate = u32at(body + 4);
            bits = u16at(body + 14);
        } else if id == b"data" {
            let end = match body.checked_add(declared) {
                Some(e) if e <= buf.len() => e,
                _ => buf.len(), // a streamed WAV declares a bogus size
            };
            if channels == 0 || rate == 0 || bits != 16 {
                return None;
            }
            return Some(Pcm::from_s16le(&buf[body..end], channels, rate));
        }
        // Chunks are word-aligned.
        pos = body + declared + (declared & 1);
    }
    None
}

/// Is ffmpeg reachable at all? Cheap enough to call before a batch.
pub fn available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a deterministic MP3 with ffmpeg's own signal generators, so
    /// the test needs no committed binary and no network.
    fn make_mp3(dir: &Path, name: &str, filter: &str, extra: &[&str]) -> std::path::PathBuf {
        let out = dir.join(name);
        let status = Command::new("ffmpeg")
            .args(["-v", "quiet", "-y", "-f", "lavfi", "-i", filter])
            .args(extra)
            .arg(&out)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "generating {name}");
        out
    }

    #[test]
    fn decodes_at_the_files_own_rate_and_channels() {
        if !available() {
            return; // CI installs ffmpeg; a dev box without it skips.
        }
        let tmp = tempfile::tempdir().unwrap();
        let mono = make_mp3(
            tmp.path(),
            "mono.mp3",
            "sine=frequency=440:duration=1:sample_rate=22050",
            &["-ac", "1"],
        );
        let p = probe(&mono).unwrap();
        assert_eq!((p.channels, p.frame_rate), (1, 22050));

        let pcm = decode(&mono).unwrap();
        assert_eq!(pcm.channels, 1);
        assert_eq!(pcm.frame_rate, 22050);
        // An MP3 decode adds encoder padding, so the length is near but
        // not exactly the requested second.
        assert!((pcm.len_ms() - 1000).abs() < 100, "{} ms", pcm.len_ms());
        // ffmpeg's sine generator is well below full scale; the point
        // is that real signal came through, not silence.
        assert!(pcm.max_abs() > 1000, "peak was {}", pcm.max_abs());
        assert!(
            pcm.dbfs() < 0.0 && pcm.dbfs() > -40.0,
            "dBFS was {}",
            pcm.dbfs()
        );
    }

    #[test]
    fn a_corrupt_file_reports_pydubs_message() {
        if !available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("bad.mp3");
        std::fs::write(&bad, b"ID3\x03\x00door").unwrap();
        let err = decode(&bad).unwrap_err().to_string();
        assert!(
            err.starts_with("Decoding failed. ffmpeg returned error code: 183"),
            "{err}"
        );
        assert!(
            err.contains("\n\nOutput from ffmpeg/avlib:\n\n"),
            "the stderr block is part of the message"
        );
    }

    #[test]
    fn silence_decodes_to_silence() {
        if !available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let quiet = make_mp3(tmp.path(), "quiet.mp3", "anullsrc=r=8000:cl=mono:d=1", &[]);
        let pcm = decode(&quiet).unwrap();
        assert_eq!(pcm.max_abs(), 0);
        assert_eq!(pcm.dbfs(), f64::NEG_INFINITY);
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        if !available() {
            return;
        }
        let err = decode(Path::new("/nonexistent/nope.mp3")).unwrap_err();
        assert!(matches!(err, AudioError::CouldntDecode { .. }), "{err}");
        assert!(
            err.to_string()
                .starts_with("Decoding failed. ffmpeg returned error code: "),
            "{err}"
        );
    }
}
