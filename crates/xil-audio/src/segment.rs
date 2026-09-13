//! pydub's `AudioSegment`, transcribed operation by operation.
//!
//! The pipeline's DAW layers are only byte-identical to Python's if every
//! quirk of pydub 0.25.1 survives, and several are load-bearing:
//!
//! * `AudioSegment.silent()` is **11025 Hz mono**. Overlaying a 44.1 kHz
//!   stereo stem onto it converts the whole layer through `ratecv`, which
//!   changes its frame count.
//! * `AudioSegment.empty()` is width 1, rate 1, mono; the first `+=` syncs
//!   it up to the appended stem.
//! * Slicing is by milliseconds through `int(ms * rate / 1000.0)`, so
//!   `seg[a:]` can drop or *pad* a few trailing frames. `overlay` is built
//!   from two such slices, so it can change a buffer's length.
//! * Negative positions count from the end, and byte slices use Python's
//!   clamping rules — a fade longer than its clip reaches both.
//!
//! Buffers are raw little-endian bytes, as pydub keeps them, so the same
//! arithmetic applies whatever the sample width.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::audioop;
use crate::ffmpeg::{self, AudioError};

/// The C library's `pow`, `log` and `log10`, called at run time.
///
/// Python's `**`, `math.log` and `math.log10` go straight to libm, so the
/// port calls the same functions rather than trusting Rust's intrinsics to
/// round identically. The last bit matters: it decides `floor()` on
/// negative samples in `pan(0.0)`.
pub mod libm {
    extern "C" {
        #[link_name = "pow"]
        fn c_pow(x: f64, y: f64) -> f64;
        #[link_name = "log"]
        fn c_log(x: f64) -> f64;
        #[link_name = "log10"]
        fn c_log10(x: f64) -> f64;
    }

    /// `math.log(x)` (natural logarithm).
    pub fn ln(x: f64) -> f64 {
        // SAFETY: pure libm function over plain doubles.
        unsafe { c_log(std::hint::black_box(x)) }
    }

    // LLVM recognises `pow`/`log10` as library functions and folds calls
    // with constant arguments at compile time, which would undo the point
    // of calling libm; `black_box` keeps the arguments opaque.
    pub fn pow(x: f64, y: f64) -> f64 {
        // SAFETY: pure libm function over plain doubles.
        unsafe { c_pow(std::hint::black_box(x), std::hint::black_box(y)) }
    }

    pub fn log10(x: f64) -> f64 {
        // SAFETY: pure libm function over plain doubles.
        unsafe { c_log10(std::hint::black_box(x)) }
    }
}

/// pydub's `db_to_float` in amplitude mode: `10 ** (db / 20)`.
pub fn db_to_float(db: f64) -> f64 {
    libm::pow(10.0, db / 20.0)
}

/// pydub's `ratio_to_db` in amplitude mode: `20 * log(ratio, 10)`, which
/// CPython evaluates as `log(ratio) / log(10)` — *not* `log10(ratio)`.
/// `20 * log(2, 10)` is `6.020599913279623`; `20 * log10(2)` is `...624`.
pub fn ratio_to_db(ratio: f64) -> f64 {
    if ratio == 0.0 {
        return f64::NEG_INFINITY;
    }
    20.0 * (libm::ln(ratio) / libm::ln(10.0))
}

/// Python's `round()` of a float to an int: ties go to even.
pub fn py_round(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 {
        let t = x.trunc();
        if (t as i64) % 2 == 0 {
            t as i64
        } else {
            r as i64
        }
    } else {
        r as i64
    }
}

/// Python byte slicing `data[a:b]`: negative indexes count from the end,
/// both ends clamp, and an inverted range is empty.
fn py_slice(data: &[u8], a: i64, b: i64) -> &[u8] {
    let n = data.len() as i64;
    let fix = |i: i64| {
        let i = if i < 0 { i + n } else { i };
        i.clamp(0, n) as usize
    };
    let (a, b) = (fix(a), fix(b));
    if b <= a {
        &[]
    } else {
        &data[a..b]
    }
}

/// Python's `//` on ints: rounds toward negative infinity.
fn floor_div(a: i64, b: i64) -> i64 {
    a.div_euclid(b) - if a.rem_euclid(b) != 0 && b < 0 { 1 } else { 0 }
}

/// An immutable-in-spirit block of interleaved PCM. Methods consume and
/// return segments the way pydub's `_spawn` hands back new objects.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub data: Vec<u8>,
    pub sample_width: usize,
    pub frame_rate: u32,
    pub channels: usize,
}

impl Segment {
    pub fn new(data: Vec<u8>, sample_width: usize, frame_rate: u32, channels: usize) -> Segment {
        Segment {
            data,
            sample_width,
            frame_rate,
            channels,
        }
    }

    fn spawn(&self, data: Vec<u8>) -> Segment {
        Segment::new(data, self.sample_width, self.frame_rate, self.channels)
    }

    pub fn frame_width(&self) -> usize {
        self.channels * self.sample_width
    }

    /// `AudioSegment.empty()`.
    pub fn empty() -> Segment {
        Segment::new(Vec::new(), 1, 1, 1)
    }

    /// `AudioSegment.silent(duration, frame_rate)`.
    pub fn silent_at(duration_ms: f64, frame_rate: u32) -> Segment {
        let frames = (frame_rate as f64 * (duration_ms / 1000.0)) as i64;
        Segment::new(vec![0u8; 2 * frames.max(0) as usize], 2, frame_rate, 1)
    }

    /// `AudioSegment.silent(duration)` — at pydub's default 11025 Hz.
    pub fn silent(duration_ms: f64) -> Segment {
        Segment::silent_at(duration_ms, 11025)
    }

    /// `frame_count()` — whole frames, as a float.
    pub fn frame_count(&self) -> f64 {
        (self.data.len() / self.frame_width()) as f64
    }

    /// `frame_count(ms=...)` — fractional frames for a duration.
    pub fn frame_count_ms(&self, ms: f64) -> f64 {
        ms * (self.frame_rate as f64 / 1000.0)
    }

    /// `len(segment)` in milliseconds.
    pub fn len_ms(&self) -> i64 {
        py_round(1000.0 * (self.frame_count() / self.frame_rate as f64))
    }

    fn parse_position(&self, val: f64) -> i64 {
        let len = self.len_ms() as f64;
        let val = if val < 0.0 { len - val.abs() } else { val };
        let frames = if val == f64::INFINITY {
            self.frame_count_ms(len)
        } else {
            self.frame_count_ms(val)
        };
        frames as i64
    }

    fn slice_frames(&self, start: f64, end: f64) -> Segment {
        let fw = self.frame_width() as i64;
        let s = self.parse_position(start) * fw;
        let e = self.parse_position(end) * fw;
        let mut data = py_slice(&self.data, s, e).to_vec();
        let expected = e - s;
        let missing = floor_div(expected - data.len() as i64, fw);
        if missing != 0 {
            if missing as f64 > self.frame_count_ms(2.0) {
                panic!(
                    "TooManyMissingFrames: You should never be filling in    more than 2 ms with silence here, missing frames: {missing}"
                );
            }
            let head = &data[..data.len().min(fw as usize)];
            let silence = audioop::mul(head, self.sample_width, 0.0);
            if missing > 0 {
                data.extend(silence.repeat(missing as usize));
            }
        }
        self.spawn(data)
    }

    /// `segment[start:end]` in milliseconds; `None` is an open end.
    pub fn slice(&self, start: Option<f64>, end: Option<f64>) -> Segment {
        let len = self.len_ms() as f64;
        let start = start.unwrap_or(0.0).min(len);
        let end = end.unwrap_or(len).min(len);
        self.slice_frames(start, end)
    }

    /// `segment[ms]` — one millisecond, deliberately *not* clamped.
    pub fn at(&self, ms: f64) -> Segment {
        self.slice_frames(ms, ms + 1.0)
    }

    /// `get_frame(index)`.
    fn get_frame(&self, index: i64) -> &[u8] {
        let fw = self.frame_width() as i64;
        py_slice(&self.data, index * fw, index * fw + fw)
    }

    /// `segment * n` for an integer `n`.
    pub fn repeat(&self, n: i64) -> Segment {
        self.spawn(self.data.repeat(n.max(0) as usize))
    }

    /// `segment + db` / `segment - db` — `apply_gain`.
    pub fn gain(&self, db: f64) -> Segment {
        self.spawn(audioop::mul(&self.data, self.sample_width, db_to_float(db)))
    }

    pub fn set_sample_width(self, sample_width: usize) -> Segment {
        if sample_width == self.sample_width {
            return self;
        }
        let data = audioop::lin2lin(&self.data, self.sample_width, sample_width);
        Segment::new(data, sample_width, self.frame_rate, self.channels)
    }

    pub fn set_frame_rate(self, frame_rate: u32) -> Segment {
        if frame_rate == self.frame_rate {
            return self;
        }
        let data = if self.data.is_empty() {
            self.data
        } else {
            audioop::ratecv(
                &self.data,
                self.sample_width,
                self.channels,
                self.frame_rate,
                frame_rate,
            )
        };
        Segment::new(data, self.sample_width, frame_rate, self.channels)
    }

    pub fn set_channels(self, channels: usize) -> Segment {
        if channels == self.channels {
            return self;
        }
        let data = match (self.channels, channels) {
            (1, 2) => audioop::tostereo(&self.data, self.sample_width, 1.0, 1.0),
            (2, 1) => audioop::tomono(&self.data, self.sample_width, 0.5, 0.5),
            (from, to) => panic!("set_channels: {from} -> {to} is not used by the pipeline"),
        };
        Segment::new(data, self.sample_width, self.frame_rate, channels)
    }

    /// `AudioSegment._sync(a, b)`.
    pub fn sync(a: Segment, b: Segment) -> (Segment, Segment) {
        let channels = a.channels.max(b.channels);
        let frame_rate = a.frame_rate.max(b.frame_rate);
        let width = a.sample_width.max(b.sample_width);
        let conv = |s: Segment| {
            s.set_channels(channels)
                .set_frame_rate(frame_rate)
                .set_sample_width(width)
        };
        (conv(a), conv(b))
    }

    /// `self + other` — `append(other, crossfade=0)`.
    pub fn append(self, other: Segment) -> Segment {
        let (mut a, b) = Segment::sync(self, other);
        a.data.extend_from_slice(&b.data);
        a
    }

    /// `overlay(seg, position)` with pydub's defaults (no loop, one pass).
    pub fn overlay(self, seg: Segment, position: i64) -> Segment {
        let (seg1, seg2) = Segment::sync(self, seg);
        let width = seg1.sample_width;
        let pos_ms = position as f64;

        // Fast path: when both of pydub's slices are exact (no dropped or
        // padded frames), its output is the base buffer with the overlay
        // summed in place, so do exactly that instead of copying.
        let fw = seg1.frame_width() as i64;
        let len = seg1.len_ms() as f64;
        let cut = seg1.parse_position(pos_ms.min(len)) * fw;
        let tail_end = seg1.parse_position(len) * fw;
        if cut >= 0 && tail_end == seg1.data.len() as i64 && cut <= tail_end {
            let mut out = seg1;
            let cut = cut as usize;
            let n = seg2.data.len().min(out.data.len() - cut);
            audioop::add_into(&mut out.data[cut..cut + n], &seg2.data[..n], width);
            return out;
        }

        let mut output = seg1.slice(None, Some(pos_ms)).data;
        let base = seg1.slice(Some(pos_ms), None).data;
        let mut overlay = seg2.data;
        let remaining = base.len();
        if overlay.len() >= remaining {
            overlay.truncate(remaining);
        }
        let n = overlay.len();
        output.extend(audioop::add(&base[..n], &overlay, width));
        output.extend_from_slice(&base[n..]);
        seg1.spawn(output)
    }

    /// `fade(to_gain, from_gain, start, end, duration)`.
    pub fn fade(
        &self,
        to_gain: f64,
        from_gain: f64,
        start: Option<f64>,
        end: Option<f64>,
        duration: Option<f64>,
    ) -> Segment {
        if to_gain == 0.0 && from_gain == 0.0 {
            return self.clone();
        }
        let len = self.len_ms() as f64;
        let mut start = start.map(|s| s.min(len));
        let mut end = end.map(|e| e.min(len));
        if let Some(s) = start {
            if s < 0.0 {
                start = Some(s + len);
            }
        }
        if let Some(e) = end {
            if e < 0.0 {
                end = Some(e + len);
            }
        }
        let duration = match duration {
            Some(d) if d != 0.0 => {
                if let Some(s) = start {
                    end = Some(s + d);
                } else if let Some(e) = end {
                    start = Some(e - d);
                }
                d
            }
            _ => end.unwrap() - start.unwrap(),
        };
        let (start, end) = (start.unwrap(), end.unwrap());

        let from_power = db_to_float(from_gain);
        let mut output = Vec::new();

        let mut before = self.slice(None, Some(start)).data;
        if from_gain != 0.0 {
            before = audioop::mul(&before, self.sample_width, from_power);
        }
        output.extend(before);

        let gain_delta = db_to_float(to_gain) - from_power;
        if duration > 100.0 {
            let scale_step = gain_delta / duration;
            for i in 0..duration as i64 {
                let volume_change = from_power + scale_step * i as f64;
                let chunk = self.at(start + i as f64);
                output.extend(audioop::mul(&chunk.data, self.sample_width, volume_change));
            }
        } else {
            let start_frame = self.frame_count_ms(start);
            let end_frame = self.frame_count_ms(end);
            let fade_frames = end_frame - start_frame;
            let scale_step = gain_delta / fade_frames;
            for i in 0..fade_frames as i64 {
                let volume_change = from_power + scale_step * i as f64;
                let sample = self.get_frame((start_frame + i as f64) as i64);
                output.extend(audioop::mul(sample, self.sample_width, volume_change));
            }
        }

        let mut after = self.slice(Some(end), None).data;
        if to_gain != 0.0 {
            after = audioop::mul(&after, self.sample_width, db_to_float(to_gain));
        }
        output.extend(after);
        self.spawn(output)
    }

    pub fn fade_in(&self, duration_ms: f64) -> Segment {
        self.fade(0.0, -120.0, Some(0.0), None, Some(duration_ms))
    }

    pub fn fade_out(&self, duration_ms: f64) -> Segment {
        self.fade(-120.0, 0.0, None, Some(f64::INFINITY), Some(duration_ms))
    }

    /// `split_to_mono()` for mono and stereo.
    pub fn split_to_mono(&self) -> Vec<Segment> {
        if self.channels == 1 {
            return vec![self.clone()];
        }
        let w = self.sample_width;
        (0..self.channels)
            .map(|c| {
                let data: Vec<u8> = self
                    .data
                    .chunks_exact(self.frame_width())
                    .flat_map(|f| f[c * w..(c + 1) * w].to_vec())
                    .collect();
                Segment::new(data, w, self.frame_rate, 1)
            })
            .collect()
    }

    /// `effects.apply_gain_stereo(left_gain, right_gain)`.
    pub fn apply_gain_stereo(&self, left_gain: f64, right_gain: f64) -> Segment {
        let (left, right) = if self.channels == 1 {
            (self.clone(), self.clone())
        } else {
            let mut parts = self.split_to_mono();
            let r = parts.pop().unwrap();
            (parts.pop().unwrap(), r)
        };
        let w = self.sample_width;
        let l = audioop::mul(&left.data, w, db_to_float(left_gain));
        let l = audioop::tostereo(&l, w, 1.0, 0.0);
        let r = audioop::mul(&right.data, w, db_to_float(right_gain));
        let r = audioop::tostereo(&r, w, 0.0, 1.0);
        Segment::new(audioop::add(&l, &r, w), w, self.frame_rate, 2)
    }

    /// `effects.pan(pan_amount)` — not equal-power; pydub's own formula.
    pub fn pan(&self, pan_amount: f64) -> Segment {
        assert!(
            (-1.0..=1.0).contains(&pan_amount),
            "pan_amount should be between -1.0 (100% left) and +1.0 (100% right)"
        );
        let max_boost_db = ratio_to_db(2.0);
        let boost_db = pan_amount.abs() * max_boost_db;
        let boost_factor = db_to_float(boost_db);
        let reduce_factor = db_to_float(max_boost_db) - boost_factor;
        let reduce_db = ratio_to_db(reduce_factor);
        let boost_db = boost_db / 2.0;
        if pan_amount < 0.0 {
            self.apply_gain_stereo(boost_db, reduce_db)
        } else {
            self.apply_gain_stereo(reduce_db, boost_db)
        }
    }

    fn samples_i16(&self) -> Vec<i16> {
        assert_eq!(
            self.sample_width, 2,
            "filters are only used on 16-bit audio"
        );
        self.data
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    fn spawn_i16(&self, s: &[i16]) -> Segment {
        self.spawn(s.iter().flat_map(|v| v.to_le_bytes()).collect())
    }

    /// `effects.low_pass_filter(cutoff)` — single pole, as pydub has it.
    // Index loops kept on purpose: this is a line-by-line transcription.
    #[allow(clippy::needless_range_loop)]
    pub fn low_pass_filter(&self, cutoff: f64) -> Segment {
        let rc = 1.0 / (cutoff * 2.0 * std::f64::consts::PI);
        let dt = 1.0 / self.frame_rate as f64;
        let alpha = dt / (rc + dt);
        let original = self.samples_i16();
        let mut filtered = original.clone();
        let frames = self.frame_count() as usize;
        let ch = self.channels;
        let mut last: Vec<f64> = vec![0.0; ch];
        for i in 0..ch.min(original.len()) {
            last[i] = original[i] as f64;
            filtered[i] = original[i];
        }
        for i in 1..frames {
            for j in 0..ch {
                let off = i * ch + j;
                last[j] += alpha * (original[off] as f64 - last[j]);
                filtered[off] = py_array_int(last[j]);
            }
        }
        self.spawn_i16(&filtered)
    }

    /// `effects.high_pass_filter(cutoff)`.
    #[allow(clippy::needless_range_loop)]
    pub fn high_pass_filter(&self, cutoff: f64) -> Segment {
        let rc = 1.0 / (cutoff * 2.0 * std::f64::consts::PI);
        let dt = 1.0 / self.frame_rate as f64;
        let alpha = rc / (rc + dt);
        let (minval, maxval) = (-32768.0f64, 32767.0f64);
        let original = self.samples_i16();
        let mut filtered = original.clone();
        let frames = self.frame_count() as usize;
        let ch = self.channels;
        let mut last: Vec<f64> = vec![0.0; ch];
        for i in 0..ch.min(original.len()) {
            last[i] = original[i] as f64;
            filtered[i] = original[i];
        }
        for i in 1..frames {
            for j in 0..ch {
                let off = i * ch + j;
                let prev = (i - 1) * ch + j;
                last[j] = alpha * (last[j] + original[off] as f64 - original[prev] as f64);
                filtered[off] = py_array_int(last[j].max(minval).min(maxval));
            }
        }
        self.spawn_i16(&filtered)
    }

    /// `AudioSegment.from_file(path)`: WAVs by name are read directly,
    /// everything else goes through ffmpeg exactly as pydub drives it.
    pub fn from_file(path: &Path) -> Result<Segment, AudioError> {
        let is_wav = path.to_string_lossy().to_lowercase().ends_with(".wav");
        if is_wav {
            if let Some(seg) = std::fs::read(path).ok().and_then(|b| read_wav(&b)) {
                return Ok(seg);
            }
        }
        let pcm = ffmpeg::decode(path)?;
        let data = pcm.samples.iter().flat_map(|v| v.to_le_bytes()).collect();
        Ok(Segment::new(data, 2, pcm.frame_rate, pcm.channels as usize))
    }

    /// `export(path, format="wav")` with no codec or parameters — Python's
    /// `wave` module writes the file, no ffmpeg involved.
    pub fn export_wav(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.wav_bytes())
    }

    /// The bytes `wave.open(f, "wb")` produces for this segment.
    pub fn wav_bytes(&self) -> Vec<u8> {
        let nframes = self.frame_count() as u32;
        let datalength = nframes * self.channels as u32 * self.sample_width as u32;
        let mut out = Vec::with_capacity(44 + self.data.len());
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + datalength).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(self.channels as u16).to_le_bytes());
        out.extend_from_slice(&self.frame_rate.to_le_bytes());
        let block_align = (self.channels * self.sample_width) as u32;
        out.extend_from_slice(&(self.frame_rate * block_align).to_le_bytes());
        out.extend_from_slice(&(block_align as u16).to_le_bytes());
        out.extend_from_slice(&((self.sample_width * 8) as u16).to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&datalength.to_le_bytes());
        if self.sample_width == 1 {
            out.extend(self.data.iter().map(|b| b.wrapping_add(128)));
        } else {
            out.extend_from_slice(&self.data);
        }
        out
    }

    /// `export(path, format=fmt, parameters=...)` through ffmpeg: a temporary
    /// WAV in, `ffmpeg -y -f wav -i tmp [params] -f fmt out` — the exact
    /// command pydub builds, so the encoded bytes match.
    pub fn export_ffmpeg(
        &self,
        path: &Path,
        format: &str,
        parameters: &[&str],
    ) -> Result<(), AudioError> {
        // Closed paths, not open handles: Windows will not let ffmpeg write
        // to a file another handle still holds.
        let tmp_in = tempfile::Builder::new()
            .suffix(".wav")
            .tempfile()
            .map_err(|e| io_err(path, e))?
            .into_temp_path();
        std::fs::write(&tmp_in, self.wav_bytes()).map_err(|e| io_err(path, e))?;
        let tmp_out = tempfile::NamedTempFile::new()
            .map_err(|e| io_err(path, e))?
            .into_temp_path();
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-y").args(["-f", "wav", "-i"]).arg(&tmp_in);
        cmd.args(parameters);
        cmd.args(["-f", format]).arg(&tmp_out);
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .map_err(|_| AudioError::FfmpegMissing)?;
        if !out.status.success() {
            return Err(AudioError::CouldntEncode {
                code: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let bytes = std::fs::read(&tmp_out).map_err(|e| io_err(path, e))?;
        std::fs::write(path, bytes).map_err(|e| io_err(path, e))
    }
}

fn io_err(path: &Path, e: std::io::Error) -> AudioError {
    AudioError::Malformed {
        path: path.display().to_string(),
        what: e.to_string(),
    }
}

/// `int(x)` stored into an `array('h')`: truncates toward zero; a value
/// outside the type would raise `OverflowError` in Python.
fn py_array_int(x: f64) -> i16 {
    let v = x as i64;
    assert!(
        (-32768..=32767).contains(&v),
        "OverflowError: signed short integer is greater than maximum"
    );
    v as i16
}

/// pydub's `read_wav_audio` (with `extract_wav_headers`): scan at most ten
/// sub-chunks up to `data`, honour its declared size, reject non-PCM.
pub fn read_wav(data: &[u8]) -> Option<Segment> {
    if data.len() < 12 {
        return None;
    }
    let u16at = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]);
    let u32at = |i: usize| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
    let mut pos = 12usize;
    let mut chunks: Vec<(&[u8], usize, usize)> = Vec::new();
    while pos + 8 <= data.len() && chunks.len() < 10 {
        let id = &data[pos..pos + 4];
        let size = u32at(pos + 4) as usize;
        chunks.push((id, pos, size));
        if id == b"data" {
            break;
        }
        pos += size + 8;
    }
    let (_, fpos, fsize) = *chunks.iter().find(|c| c.0 == b"fmt ")?;
    if fsize < 16 || fpos + 24 > data.len() {
        return None;
    }
    let p = fpos + 8;
    let audio_format = u16at(p);
    if audio_format != 1 && audio_format != 0xFFFE {
        return None;
    }
    let channels = u16at(p + 2) as usize;
    let rate = u32at(p + 4);
    let bits = u16at(p + 14) as usize;
    let &(id, dpos, dsize) = chunks.last()?;
    if id != b"data" {
        return None;
    }
    let start = dpos + 8;
    let end = (start + dsize).min(data.len());
    let width = bits / 8;
    if !(width == 1 || width == 2 || width == 4) || channels == 0 {
        return None;
    }
    let mut raw = data[start.min(end)..end].to_vec();
    if width == 1 {
        raw.iter_mut().for_each(|b| *b = b.wrapping_sub(128));
    }
    Some(Segment::new(raw, width, rate, channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(v: &[i16], rate: u32, ch: usize) -> Segment {
        Segment::new(
            v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            2,
            rate,
            ch,
        )
    }
    fn vals(s: &Segment) -> Vec<i16> {
        s.samples_i16()
    }

    #[test]
    fn silent_and_empty_have_pydubs_defaults() {
        let s = Segment::silent(600.0);
        assert_eq!((s.frame_rate, s.channels, s.sample_width), (11025, 1, 2));
        assert_eq!(s.frame_count(), 6615.0);
        assert_eq!(s.len_ms(), 600);
        let e = Segment::empty();
        assert_eq!((e.frame_rate, e.channels, e.sample_width), (1, 1, 1));
    }

    #[test]
    fn appending_to_empty_syncs_up() {
        let a = seg(&[1, 2, 3, 4], 1000, 2);
        let out = Segment::empty().append(a.clone());
        assert_eq!(out, a);
    }

    #[test]
    fn python_slicing_rules() {
        assert_eq!(py_slice(b"abcdef", -2, 100), b"ef");
        assert_eq!(py_slice(b"abcdef", 4, 2), b"");
        assert_eq!(floor_div(-3, 2), -2);
        assert_eq!(floor_div(3, 2), 1);
        assert_eq!(py_round(2.5), 2);
        assert_eq!(py_round(3.5), 4);
        assert_eq!(py_round(1234.4), 1234);
    }

    #[test]
    fn overlay_truncates_at_base_end() {
        let base = seg(&[10, 10, 10, 10], 1000, 1);
        let top = seg(&[1, 2, 3], 1000, 1);
        assert_eq!(
            vals(&base.clone().overlay(top.clone(), 2)),
            [10, 10, 11, 12]
        );
        assert_eq!(vals(&base.overlay(top, 0)), [11, 12, 13, 10]);
    }

    #[test]
    fn pan_centre_keeps_levels_close() {
        let s = seg(&[1000, -1000], 44100, 1);
        let p = s.pan(0.0);
        assert_eq!(p.channels, 2);
        let v = vals(&p);
        assert!(v.iter().all(|x| (x.abs() - 1000).abs() <= 1), "{v:?}");
    }

    #[test]
    fn wav_round_trip() {
        let s = seg(&[1, -2, 3, -4], 8000, 2);
        let bytes = s.wav_bytes();
        assert_eq!(bytes.len(), 44 + 8);
        assert_eq!(read_wav(&bytes).unwrap(), s);
    }
}
