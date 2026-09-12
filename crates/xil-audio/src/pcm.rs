//! Interleaved 16-bit PCM with the measurements pydub exposes.
//!
//! Every formula here is pydub's, which is `audioop`'s underneath, and the
//! integer truncation is load-bearing: `rms` is an int, so `dBFS` is the
//! log of a rounded-down value, not of the true RMS. Reproducing the
//! rounding matters more than being more accurate.

/// Sample width is always 2 in this pipeline; pydub's
/// `max_possible_amplitude` is `2^16 / 2`.
pub const MAX_POSSIBLE_AMPLITUDE: f64 = 32768.0;

/// Interleaved `i16` samples plus the format they were decoded at.
#[derive(Clone, Debug, PartialEq)]
pub struct Pcm {
    pub samples: Vec<i16>,
    pub channels: u16,
    pub frame_rate: u32,
}

impl Pcm {
    pub fn new(samples: Vec<i16>, channels: u16, frame_rate: u32) -> Pcm {
        Pcm {
            samples,
            channels,
            frame_rate,
        }
    }

    /// Decode raw little-endian `s16le` bytes. A trailing odd byte is
    /// dropped, as reading 16-bit frames necessarily does.
    pub fn from_s16le(bytes: &[u8], channels: u16, frame_rate: u32) -> Pcm {
        let samples = bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        Pcm::new(samples, channels, frame_rate)
    }

    /// Frames, i.e. samples divided by channels (`len(_data) // frame_width`).
    pub fn frame_count(&self) -> usize {
        if self.channels == 0 {
            return 0;
        }
        self.samples.len() / self.channels as usize
    }

    /// `len(segment)` — duration in whole milliseconds, rounded.
    pub fn len_ms(&self) -> i64 {
        if self.frame_rate == 0 {
            return 0;
        }
        py_round(1000.0 * self.frame_count() as f64 / self.frame_rate as f64)
    }

    /// `audioop.rms` — `int(sqrt(sum(v²)/n))`, accumulated in f64 and
    /// truncated toward zero.
    pub fn rms(&self) -> i64 {
        if self.samples.is_empty() {
            return 0;
        }
        let sum_squares: f64 = self.samples.iter().map(|&v| (v as f64) * (v as f64)).sum();
        (sum_squares / self.samples.len() as f64).sqrt() as i64
    }

    /// `audioop.max` — the largest absolute sample. `-32768` yields 32768,
    /// which is why this is not an `i16`.
    pub fn max_abs(&self) -> i64 {
        self.samples
            .iter()
            .map(|&v| (v as i64).abs())
            .max()
            .unwrap_or(0)
    }

    /// `segment.dBFS` — RMS relative to full scale. `-inf` for silence.
    pub fn dbfs(&self) -> f64 {
        ratio_to_db(self.rms() as f64 / MAX_POSSIBLE_AMPLITUDE)
    }

    /// `segment.max_dBFS` — peak sample relative to full scale.
    pub fn max_dbfs(&self) -> f64 {
        ratio_to_db(self.max_abs() as f64 / MAX_POSSIBLE_AMPLITUDE)
    }

    /// `segment[start_ms:end_ms]`. Both ends clamp to the segment length,
    /// and the frame index is `int(ms * rate / 1000)`.
    ///
    /// pydub pads a short tail with silence so the slice is the length the
    /// caller asked for; that only happens mid-buffer, never at the end,
    /// so slicing the final partial chunk returns what is actually there.
    pub fn slice_ms(&self, start_ms: i64, end_ms: i64) -> Pcm {
        let total_ms = self.len_ms();
        let start = self.frame_for_ms(start_ms.min(total_ms));
        let end = self.frame_for_ms(end_ms.min(total_ms));
        let ch = self.channels.max(1) as usize;
        let lo = (start * ch).min(self.samples.len());
        let hi = (end * ch).min(self.samples.len()).max(lo);
        Pcm::new(
            self.samples[lo..hi].to_vec(),
            self.channels,
            self.frame_rate,
        )
    }

    /// `_parse_position` — `int(ms * frame_rate / 1000.0)`.
    fn frame_for_ms(&self, ms: i64) -> usize {
        if ms <= 0 {
            return 0;
        }
        (ms as f64 * (self.frame_rate as f64 / 1000.0)) as usize
    }
}

/// `pydub.utils.ratio_to_db` in amplitude mode: `20 * log10(ratio)`,
/// `-inf` at zero.
pub fn ratio_to_db(ratio: f64) -> f64 {
    if ratio == 0.0 {
        return f64::NEG_INFINITY;
    }
    20.0 * ratio.log10()
}

/// Python's `round()` — half away from zero is *not* what it does; it is
/// banker's rounding, so 0.5 goes to 0 and 1.5 goes to 2.
fn py_round(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        (r - x.signum()) as i64
    } else {
        r as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: i16) -> Pcm {
        Pcm::new(
            (0..n)
                .map(|i| if i % 2 == 0 { amp } else { -amp })
                .collect(),
            1,
            1000,
        )
    }

    #[test]
    fn silence_reads_as_negative_infinity() {
        let p = Pcm::new(vec![0; 100], 1, 1000);
        assert_eq!(p.rms(), 0);
        assert_eq!(p.max_abs(), 0);
        assert_eq!(p.dbfs(), f64::NEG_INFINITY);
        assert_eq!(p.max_dbfs(), f64::NEG_INFINITY);
    }

    #[test]
    fn full_scale_square_wave_is_zero_dbfs() {
        let p = tone(100, 32767);
        assert_eq!(p.rms(), 32767);
        assert_eq!(p.max_abs(), 32767);
        // 32767/32768 is a hair under full scale.
        assert!((p.dbfs() - -0.000265).abs() < 1e-5, "{}", p.dbfs());
    }

    #[test]
    fn rms_truncates_like_audioop() {
        // sum of squares = 2*(100^2) = 20000, mean 10000, sqrt exactly 100.
        assert_eq!(Pcm::new(vec![100, -100], 1, 1000).rms(), 100);
        // mean 16250, sqrt 127.475… → truncated to 127, matching audioop.
        assert_eq!(Pcm::new(vec![100, -150], 1, 1000).rms(), 127);
    }

    #[test]
    fn most_negative_sample_does_not_overflow() {
        let p = Pcm::new(vec![-32768, 0], 1, 1000);
        assert_eq!(p.max_abs(), 32768, "abs(-32768) does not fit in an i16");
        assert_eq!(p.max_dbfs(), 0.0, "exactly full scale");
    }

    #[test]
    fn duration_counts_frames_not_samples() {
        let stereo = Pcm::new(vec![0; 2000], 2, 1000);
        assert_eq!(stereo.frame_count(), 1000);
        assert_eq!(stereo.len_ms(), 1000);
        let mono = Pcm::new(vec![0; 2000], 1, 1000);
        assert_eq!(mono.len_ms(), 2000);
        assert_eq!(Pcm::new(vec![], 2, 44100).len_ms(), 0);
    }

    #[test]
    fn slices_clamp_and_index_by_frame() {
        let p = Pcm::new((0..1000i16).collect(), 1, 1000); // 1000 frames, 1000 ms
        let s = p.slice_ms(0, 500);
        assert_eq!(s.samples.len(), 500);
        assert_eq!(s.samples[0], 0);
        let tail = p.slice_ms(900, 1400);
        assert_eq!(tail.samples.len(), 100, "the end clamps, it does not pad");
        assert_eq!(tail.samples[0], 900);
        assert!(p.slice_ms(2000, 3000).samples.is_empty());
    }

    #[test]
    fn s16le_round_trip() {
        let bytes = [0x01, 0x00, 0xff, 0xff, 0x00, 0x80];
        let p = Pcm::from_s16le(&bytes, 1, 8000);
        assert_eq!(p.samples, vec![1, -1, -32768]);
        // A dangling byte cannot form a sample and is dropped.
        assert_eq!(
            Pcm::from_s16le(&[0x01, 0x00, 0x02], 1, 8000).samples,
            vec![1]
        );
    }

    #[test]
    fn python_round_is_banker_s() {
        assert_eq!(py_round(0.5), 0);
        assert_eq!(py_round(1.5), 2);
        assert_eq!(py_round(2.5), 2);
        assert_eq!(py_round(-0.5), 0);
        assert_eq!(py_round(-1.5), -2);
        assert_eq!(py_round(2.4), 2);
        assert_eq!(py_round(2.6), 3);
    }
}
