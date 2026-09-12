//! MP3 stream info the way mutagen computes it. A transcription of
//! `mutagen.mp3.MPEGInfo` and `mutagen.mp3._util`.
//!
//! The Python pipeline reads durations and bit rates from mutagen headers,
//! not from a decode, and mutagen disagrees with ffprobe on roughly one
//! file in six of the real library (encoder delay handling, LAME padding,
//! CBR estimates). A report that says `2.4s` where Python says `2.5s`
//! fails parity, so the arithmetic here follows mutagen line for line,
//! including its file-size fallback and its half-to-even bit rate rounding.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// What mutagen raises, by the name Python code prints via
/// `type(exc).__name__` — that name reaches CSV output in `sfx-impact`.
#[derive(Debug, thiserror::Error)]
pub enum MpegError {
    #[error("can't sync to MPEG frame")]
    HeaderNotFound,
    /// Any OS error opening or reading. mutagen wraps these in its own
    /// `MutagenError`, so the Python name is the same for all of them.
    #[error("{0}")]
    Mutagen(String),
}

impl MpegError {
    /// The `type(exc).__name__` Python would print.
    pub fn python_name(&self) -> &'static str {
        match self {
            MpegError::HeaderNotFound => "HeaderNotFoundError",
            MpegError::Mutagen(_) => "MutagenError",
        }
    }
}

impl From<io::Error> for MpegError {
    fn from(e: io::Error) -> Self {
        MpegError::Mutagen(e.to_string())
    }
}

/// `MPEGInfo`'s useful attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct MpegInfo {
    /// Audio length in seconds.
    pub length: f64,
    /// Bits per second; an estimate from the first frame for CBR files.
    pub bitrate: u64,
    pub sample_rate: u32,
    pub channels: u16,
    /// mutagen's "may not be valid MPEG audio" flag.
    pub sketchy: bool,
}

/// Per-`(version, layer)` bit rate tables in kbps, index by the 4-bit field.
fn bitrate_table(version: Version, layer: u8) -> &'static [u32; 15] {
    const V1L1: [u32; 15] = [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    const V1L2: [u32; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const V1L3: [u32; 15] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const V2L1: [u32; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    const V2L23: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    match (version, layer) {
        (Version::V1, 1) => &V1L1,
        (Version::V1, 2) => &V1L2,
        (Version::V1, _) => &V1L3,
        (_, 1) => &V2L1,
        _ => &V2L23,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Version {
    V1,
    V2,
    V25,
}

fn sample_rates(version: Version) -> [u32; 3] {
    match version {
        Version::V1 => [44100, 48000, 32000],
        Version::V2 => [22050, 24000, 16000],
        Version::V25 => [11025, 12000, 8000],
    }
}

/// One parsed frame header plus whatever the Xing/VBRI/LAME tags in it
/// said. `length` is `None` where mutagen leaves the attribute unset.
struct Frame {
    frame_offset: u64,
    /// Where the next frame starts.
    next_offset: u64,
    bitrate: u64,
    sample_rate: u32,
    channels: u16,
    sketchy: bool,
    length: Option<f64>,
}

fn read_exact_at<R: Read + Seek>(f: &mut R, pos: u64, n: usize) -> io::Result<Option<Vec<u8>>> {
    f.seek(SeekFrom::Start(pos))?;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        let k = f.read(&mut buf[got..])?;
        if k == 0 {
            break;
        }
        got += k;
    }
    Ok(if got == n { Some(buf) } else { None })
}

fn u32be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn u16be(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// `skip_id3`: step over every leading `ID3` tag (WMP stacks them).
fn skip_id3<R: Read + Seek>(f: &mut R, mut pos: u64) -> io::Result<u64> {
    loop {
        let Some(h) = read_exact_at(f, pos, 10)? else {
            return Ok(pos);
        };
        // BitPaddedInt over the 4 syncsafe size bytes.
        let insize = h[6..10]
            .iter()
            .fold(0u64, |acc, b| (acc << 7) | u64::from(b & 0x7f));
        if &h[0..3] == b"ID3" && insize > 0 {
            pos += 10 + insize;
        } else {
            return Ok(pos);
        }
    }
}

/// Parse the frame at `pos`. `None` is mutagen's `HeaderNotFoundError`.
fn parse_frame<R: Read + Seek>(f: &mut R, pos: u64) -> io::Result<Option<Frame>> {
    let Some(h) = read_exact_at(f, pos, 4)? else {
        return Ok(None); // truncated header
    };
    let bits = u32be(&h);
    if bits >> 21 != 0x7ff {
        return Ok(None); // invalid sync
    }
    let version = (bits >> 19) & 0x3;
    let layer_bits = (bits >> 17) & 0x3;
    let bitrate_idx = (bits >> 12) & 0xf;
    let sr_idx = (bits >> 10) & 0x3;
    let padding = (bits >> 9) & 0x1;
    let mode = (bits >> 6) & 0x3;

    // Strict, to keep false positives down.
    if version == 1 || layer_bits == 0 || sr_idx == 0x3 || bitrate_idx == 0xf || bitrate_idx == 0 {
        return Ok(None);
    }
    let version = match version {
        0 => Version::V25,
        2 => Version::V2,
        _ => Version::V1,
    };
    let layer = (4 - layer_bits) as u8;
    let channels: u16 = if mode == 3 { 1 } else { 2 };
    let bitrate = u64::from(bitrate_table(version, layer)[bitrate_idx as usize]) * 1000;
    let sample_rate = sample_rates(version)[sr_idx as usize];

    let (frame_size, slot): (u64, u64) = if layer == 1 {
        (384, 4)
    } else if version != Version::V1 && layer == 3 {
        (576, 1)
    } else {
        (1152, 1)
    };
    let frame_length =
        ((frame_size / 8 * bitrate) / u64::from(sample_rate) + u64::from(padding)) * slot;

    let mut frame = Frame {
        frame_offset: pos,
        next_offset: pos + frame_length,
        bitrate,
        sample_rate,
        channels,
        sketchy: true,
        length: None,
    };
    if layer == 3 {
        parse_vbr_header(f, &mut frame, version, mode, frame_size, frame_length)?;
    }
    Ok(Some(frame))
}

/// A Xing/Info tag trumps the header's bit rate and gives a real length;
/// failing that, a Fraunhofer VBRI tag. Neither raises.
fn parse_vbr_header<R: Read + Seek>(
    f: &mut R,
    frame: &mut Frame,
    version: Version,
    mode: u32,
    frame_size: u64,
    frame_length: u64,
) -> io::Result<()> {
    let xing_offset: u64 = match (version, mode) {
        (Version::V1, m) if m != 3 => 36,
        (Version::V1, _) => 21,
        (_, m) if m != 3 => 21,
        _ => 13,
    };
    if let Some(xing) = parse_xing(f, frame.frame_offset + xing_offset)? {
        frame.sketchy = false;
        if let Some(frames) = xing.frames {
            let mut samples = frame_size as i64 * i64::from(frames);
            if let Some(bytes) = xing.bytes {
                if samples > 0 {
                    // The first frame is counted in xing.bytes but not in
                    // xing.frames.
                    let audio_bytes = (i64::from(bytes) - frame_length as i64).max(0);
                    let v =
                        (audio_bytes as f64 * 8.0 * f64::from(frame.sample_rate)) / samples as f64;
                    frame.bitrate = intround(v);
                }
            }
            if let Some((delay, padding)) = xing.lame_delay_padding {
                samples -= i64::from(delay);
                samples -= i64::from(padding);
            }
            if samples < 0 {
                // Older LAME wrote bogus delay/padding for short low-bitrate
                // files.
                samples = 0;
            }
            frame.length = Some(samples as f64 / f64::from(frame.sample_rate));
        }
        return Ok(());
    }

    if let Some((vbri_bytes, vbri_frames)) = parse_vbri(f, frame.frame_offset + 36)? {
        frame.sketchy = false;
        let length = (frame_size * u64::from(vbri_frames)) as f64 / f64::from(frame.sample_rate);
        frame.length = Some(length);
        if length != 0.0 {
            frame.bitrate = ((f64::from(vbri_bytes) * 8.0) / length) as u64;
        }
    }
    Ok(())
}

/// `intround`: `Decimal.from_float(v).to_integral_value(ROUND_HALF_EVEN)`.
fn intround(v: f64) -> u64 {
    let r = v.round_ties_even();
    if r < 0.0 {
        0
    } else {
        r as u64
    }
}

struct Xing {
    frames: Option<u32>,
    bytes: Option<u32>,
    /// `(encoder_delay_start, encoder_padding_end)` from a LAME info tag.
    lame_delay_padding: Option<(u32, u32)>,
}

fn parse_xing<R: Read + Seek>(f: &mut R, pos: u64) -> io::Result<Option<Xing>> {
    let Some(head) = read_exact_at(f, pos, 8)? else {
        return Ok(None);
    };
    if &head[0..4] != b"Xing" && &head[0..4] != b"Info" {
        return Ok(None);
    }
    let flags = u32be(&head[4..8]);
    let mut cur = pos + 8;
    let mut take = |f: &mut R, n: usize| -> io::Result<Option<Vec<u8>>> {
        let r = read_exact_at(f, cur, n)?;
        cur += n as u64;
        Ok(r)
    };
    let mut xing = Xing {
        frames: None,
        bytes: None,
        lame_delay_padding: None,
    };
    if flags & 0x1 != 0 {
        let Some(b) = take(f, 4)? else {
            return Ok(None); // "Xing header truncated"
        };
        xing.frames = Some(u32be(&b));
    }
    if flags & 0x2 != 0 {
        let Some(b) = take(f, 4)? else {
            return Ok(None);
        };
        xing.bytes = Some(u32be(&b));
    }
    if flags & 0x4 != 0 && take(f, 100)?.is_none() {
        return Ok(None);
    }
    if flags & 0x8 != 0 && take(f, 4)?.is_none() {
        return Ok(None);
    }
    // LAME version string, then (for LAME >= 3.90) the 27-byte info tag
    // starting 9 bytes after the "LAME"/"L3.99" marker. Any parse failure
    // here is mutagen's LAMEError, swallowed: no delay/padding correction.
    xing.lame_delay_padding = parse_lame(f, cur)?;
    Ok(Some(xing))
}

/// `LAMEHeader.parse_version` + the two fields of `LAMEHeader` the length
/// needs. `None` wherever mutagen raises `LAMEError` or reports no
/// extended header.
fn parse_lame<R: Read + Seek>(f: &mut R, pos: u64) -> io::Result<Option<(u32, u32)>> {
    let Some(data) = read_exact_at(f, pos, 20)? else {
        return Ok(None);
    };
    if !(data.starts_with(b"LAME") || data.starts_with(b"L3.99")) {
        return Ok(None);
    }
    // data.lstrip(b"EMAL")
    let mut i = 0;
    while i < data.len() && matches!(data[i], b'E' | b'M' | b'A' | b'L') {
        i += 1;
    }
    let major = data.get(i).copied();
    i += 1;
    // .lstrip(b".")
    while i < data.len() && data[i] == b'.' {
        i += 1;
    }
    let minor_start = i;
    while i < data.len() && data[i].is_ascii_digit() {
        i += 1;
    }
    let minor_digits = &data[minor_start..i];
    let major = match major {
        Some(c) if c.is_ascii_digit() => u32::from(c - b'0'),
        _ => return Ok(None),
    };
    if minor_digits.is_empty() {
        return Ok(None);
    }
    let minor: u32 = std::str::from_utf8(minor_digits)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let rest = &data[i.min(data.len())..];

    // Pre-3.90 LAME (and "LAME3.90 (alpha)") has no extended header.
    let paren = rest.len() >= 11 && data.get(9) == Some(&b'(');
    if (major, minor) < (3, 90) || ((major, minor) == (3, 90) && paren) {
        return Ok(None);
    }
    if rest.len() < 11 {
        return Ok(None); // "Invalid version: too long"
    }
    // The extended header starts 11 bytes back from the end of the 20 read.
    let Some(payload) = read_exact_at(f, pos + 9, 27)? else {
        return Ok(None); // "Not enough data"
    };
    if payload[0] >> 4 != 0 {
        return Ok(None); // unsupported header revision
    }
    let delay = (u32::from(payload[12]) << 4) | (u32::from(payload[13]) >> 4);
    let padding = ((u32::from(payload[13]) & 0xf) << 8) | u32::from(payload[14]);
    Ok(Some((delay, padding)))
}

/// `VBRIHeader`: `(bytes, frames)` when a valid tag sits at `pos`.
fn parse_vbri<R: Read + Seek>(f: &mut R, pos: u64) -> io::Result<Option<(u32, u32)>> {
    let Some(d) = read_exact_at(f, pos, 26)? else {
        return Ok(None);
    };
    if !d.starts_with(b"VBRI") || u16be(&d[4..6]) != 1 {
        return Ok(None);
    }
    let bytes = u32be(&d[10..14]);
    let frames = u32be(&d[14..18]);
    let toc_entries = u64::from(u16be(&d[18..20]));
    let toc_entry_size = u16be(&d[22..24]);
    if toc_entry_size != 2 && toc_entry_size != 4 {
        return Ok(None);
    }
    let toc_size = (toc_entries * u64::from(toc_entry_size)) as usize;
    if read_exact_at(f, pos + 26, toc_size)?.is_none() {
        return Ok(None); // "VBRI header truncated"
    }
    Ok(Some((bytes, frames)))
}

/// `MPEGInfo(fileobj)`: sync to the first believable frame run and derive
/// length and bit rate from it.
pub fn info_from<R: Read + Seek>(f: &mut R) -> Result<MpegInfo, MpegError> {
    let start = skip_id3(f, 0)?;

    // iter_sync: every 0xFF 0xEx pair in the first 1 MiB after the tags.
    const MAX_READ: usize = 1024 * 1024;
    const MAX_SYNCS: usize = 1500;
    const ENOUGH_FRAMES: usize = 4;
    const MIN_FRAMES: usize = 2;

    f.seek(SeekFrom::Start(start))?;
    let mut region = Vec::with_capacity(MAX_READ.min(1 << 16));
    f.by_ref().take(MAX_READ as u64).read_to_end(&mut region)?;

    let mut sketchy = true;
    let mut first_frame: Option<Frame> = None;
    let mut syncs_left = MAX_SYNCS;
    let mut frames: Vec<Frame> = Vec::new();

    let mut i = 0;
    while i + 1 < region.len() {
        if !(region[i] == 0xff && region[i + 1] & 0xe0 == 0xe0) {
            i += 1;
            continue;
        }
        let sync_pos = start + i as u64;
        i += 1;

        syncs_left -= 1;
        if syncs_left == 0 {
            break;
        }

        let mut next = sync_pos;
        for _ in 0..ENOUGH_FRAMES {
            let Some(fr) = parse_frame(f, next)? else {
                break;
            };
            next = fr.next_offset;
            let stop = !fr.sketchy;
            frames.push(fr);
            if stop {
                break;
            }
        }

        if frames.len() >= MIN_FRAMES && first_frame.is_none() {
            first_frame = Some(clone_frame(&frames[0]));
        }
        if frames.last().is_some_and(|fr| !fr.sketchy) {
            first_frame = frames.pop();
            sketchy = false;
            break;
        }
        if frames.len() >= ENOUGH_FRAMES {
            first_frame = Some(frames.swap_remove(0));
            sketchy = false;
            break;
        }
        frames.clear();
    }

    let Some(first) = first_frame else {
        return Err(MpegError::HeaderNotFound);
    };

    let length = match first.length {
        Some(l) => l,
        None => {
            // No length in the stream: estimate from the file size.
            let end = f.seek(SeekFrom::End(0))?;
            let content_size = end.saturating_sub(first.frame_offset);
            8.0 * content_size as f64 / first.bitrate as f64
        }
    };
    Ok(MpegInfo {
        length,
        bitrate: first.bitrate,
        sample_rate: first.sample_rate,
        channels: first.channels,
        sketchy,
    })
}

fn clone_frame(fr: &Frame) -> Frame {
    Frame {
        frame_offset: fr.frame_offset,
        next_offset: fr.next_offset,
        bitrate: fr.bitrate,
        sample_rate: fr.sample_rate,
        channels: fr.channels,
        sketchy: fr.sketchy,
        length: fr.length,
    }
}

/// `MP3(path).info`.
pub fn info(path: &Path) -> Result<MpegInfo, MpegError> {
    let mut f = File::open(path)?;
    info_from(&mut f)
}

/// `_mp3_duration_ms`: `int(info.length * 1000)`.
pub fn duration_ms(path: &Path) -> Result<i64, MpegError> {
    Ok((info(path)?.length * 1000.0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn ffmpeg_available() -> bool {
        crate::ffmpeg::available()
    }

    fn gen(dir: &Path, name: &str, rate: u32, ch: u32, extra: &[&str]) -> std::path::PathBuf {
        let out = dir.join(name);
        let filt = format!("sine=frequency=440:duration=2.3:sample_rate={rate}");
        let st = Command::new("ffmpeg")
            .args(["-v", "quiet", "-y", "-f", "lavfi", "-i", &filt, "-ac"])
            .arg(ch.to_string())
            .args(extra)
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        out
    }

    /// Expected values were printed by mutagen 1.47 on files generated with
    /// exactly these ffmpeg commands.
    #[test]
    #[allow(clippy::type_complexity)]
    fn matches_mutagen_on_lame_encoded_tones() {
        if !ffmpeg_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let cases: [(&str, u32, u32, &[&str], f64, u64, bool); 5] = [
            (
                "stereo44.mp3",
                44100,
                2,
                &[],
                2.3510204081632655,
                127999,
                false,
            ),
            (
                "mono44.mp3",
                44100,
                1,
                &[],
                2.3510204081632655,
                63999,
                false,
            ),
            (
                "mono22.mp3",
                22050,
                1,
                &[],
                2.3771428571428572,
                32001,
                false,
            ),
            (
                "stereo11.mp3",
                11025,
                2,
                &[],
                2.455510204081633,
                32000,
                false,
            ),
            (
                "noxing.mp3",
                44100,
                2,
                &["-write_xing", "0"],
                2.351,
                128000,
                false,
            ),
        ];
        for (name, rate, ch, extra, len, br, sk) in cases {
            let p = gen(d, name, rate, ch, extra);
            let i = info(&p).unwrap();
            assert_eq!(i.length, len, "{name} length");
            assert_eq!(i.bitrate, br, "{name} bitrate");
            assert_eq!(i.sketchy, sk, "{name} sketchy");
            assert_eq!(i.channels, ch as u16, "{name} channels");
            assert_eq!(i.sample_rate, rate, "{name} rate");
        }
    }

    #[test]
    fn non_audio_is_header_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("empty.mp3", &b""[..]),
            ("fake.mp3", b"ID3\x03\x00door"),
            ("text.mp3", b"hello\n"),
        ] {
            let p = tmp.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            let e = info(&p).unwrap_err();
            assert_eq!(e.python_name(), "HeaderNotFoundError", "{name}");
            assert_eq!(e.to_string(), "can't sync to MPEG frame");
        }
        let e = info(&tmp.path().join("missing.mp3")).unwrap_err();
        assert_eq!(e.python_name(), "MutagenError");
        let e = info(tmp.path()).unwrap_err();
        assert_eq!(e.python_name(), "MutagenError");
    }

    #[test]
    fn duration_ms_truncates_like_int() {
        if !ffmpeg_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let p = gen(tmp.path(), "s.mp3", 44100, 2, &[]);
        assert_eq!(duration_ms(&p).unwrap(), 2351);
    }

    #[test]
    fn intround_is_half_to_even() {
        assert_eq!(intround(2.5), 2);
        assert_eq!(intround(3.5), 4);
        assert_eq!(intround(127998.7), 127999);
    }

    /// On-demand sweep against a real library. Set `XIL_MPEG_PROBE_DIR`
    /// and it writes `path\tlength\tbitrate` lines to `XIL_MPEG_PROBE_OUT`
    /// for an offline diff against mutagen.
    #[test]
    #[ignore]
    fn probe_library() {
        let Ok(dir) = std::env::var("XIL_MPEG_PROBE_DIR") else {
            return;
        };
        let out = std::env::var("XIL_MPEG_PROBE_OUT").unwrap();
        let mut lines = Vec::new();
        fn walk(d: &Path, acc: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, acc);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp3")) {
                    acc.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(Path::new(&dir), &mut files);
        files.sort();
        for p in files {
            let line = match info(&p) {
                Ok(i) => format!("{}\t{:?}\t{}", p.display(), i.length, i.bitrate),
                Err(e) => format!("{}\tERR\t{}", p.display(), e.python_name()),
            };
            lines.push(line);
        }
        std::fs::write(out, lines.join("\n") + "\n").unwrap();
    }
}
