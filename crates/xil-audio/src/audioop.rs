//! The `audioop` primitives pydub is built on, transcribed from CPython's
//! `Modules/audioop.c` (shipped for Python 3.13+ as `audioop-lts`).
//!
//! Every conversion keeps the C code's arithmetic: products are taken in
//! `double`, clamped with `fbound` and then *floored* (not truncated), so
//! `-3 * 0.5` is `-2`. `ratecv` interpolates on 32-bit-shifted samples and
//! casts with C's truncating `(int)`. Those two rounding rules are the
//! difference between a byte-identical DAW layer and one that is merely
//! close, so nothing here is "tidied".
//!
//! Buffers are raw little-endian bytes of `width` 1, 2 or 4, as in C.

/// Smallest and largest sample for a width, as `double`s like the C tables.
fn bounds(width: usize) -> (f64, f64) {
    match width {
        1 => (-128.0, 127.0),
        2 => (-32768.0, 32767.0),
        4 => (-2147483648.0, 2147483647.0),
        _ => panic!("audioop: unsupported sample width {width}"),
    }
}

fn int_bounds(width: usize) -> (i64, i64) {
    match width {
        1 => (-128, 127),
        2 => (-32768, 32767),
        4 => (i32::MIN as i64, i32::MAX as i64),
        _ => panic!("audioop: unsupported sample width {width}"),
    }
}

/// `fbound`: clamp into range, with C's `val < minval + 1` lower test.
fn fbound(val: f64, minval: f64, maxval: f64) -> f64 {
    if val > maxval {
        maxval
    } else if val < minval + 1.0 {
        minval
    } else {
        val
    }
}

/// `GETRAWSAMPLE` — the signed sample at byte offset `pos`.
pub fn get_raw(data: &[u8], width: usize, pos: usize) -> i32 {
    match width {
        1 => data[pos] as i8 as i32,
        2 => i16::from_le_bytes([data[pos], data[pos + 1]]) as i32,
        4 => i32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]),
        _ => panic!("audioop: unsupported sample width {width}"),
    }
}

/// `SETRAWSAMPLE` — append one sample, truncating to the width.
pub fn put_raw(out: &mut Vec<u8>, width: usize, val: i32) {
    match width {
        1 => out.push(val as i8 as u8),
        2 => out.extend_from_slice(&(val as i16).to_le_bytes()),
        4 => out.extend_from_slice(&val.to_le_bytes()),
        _ => panic!("audioop: unsupported sample width {width}"),
    }
}

/// `GETSAMPLE32` — a sample scaled to occupy the top of an `int`.
fn get_sample32(data: &[u8], width: usize, pos: usize) -> i32 {
    match width {
        1 => (data[pos] as i8 as i32) << 24,
        2 => (i16::from_le_bytes([data[pos], data[pos + 1]]) as i32) << 16,
        4 => get_raw(data, 4, pos),
        _ => panic!("audioop: unsupported sample width {width}"),
    }
}

/// `SETSAMPLE32` — the inverse, with an arithmetic shift.
fn put_sample32(out: &mut Vec<u8>, width: usize, val: i32) {
    match width {
        1 => out.push((val >> 24) as i8 as u8),
        2 => out.extend_from_slice(&((val >> 16) as i16).to_le_bytes()),
        4 => out.extend_from_slice(&val.to_le_bytes()),
        _ => panic!("audioop: unsupported sample width {width}"),
    }
}

fn check_len(len: usize, width: usize) {
    assert!(
        len % width == 0,
        "audioop.error: not a whole number of frames"
    );
}

/// `audioop.mul(fragment, width, factor)`.
pub fn mul(data: &[u8], width: usize, factor: f64) -> Vec<u8> {
    check_len(data.len(), width);
    let (lo, hi) = bounds(width);
    let mut out = Vec::with_capacity(data.len());
    for pos in (0..data.len()).step_by(width) {
        let val = get_raw(data, width, pos) as f64;
        put_raw(&mut out, width, fbound(val * factor, lo, hi).floor() as i32);
    }
    out
}

/// `audioop.add(fragment1, fragment2, width)` — saturating sum.
pub fn add(a: &[u8], b: &[u8], width: usize) -> Vec<u8> {
    check_len(a.len(), width);
    assert_eq!(
        a.len(),
        b.len(),
        "audioop.error: Lengths should be the same"
    );
    let (lo, hi) = int_bounds(width);
    let mut out = Vec::with_capacity(a.len());
    for pos in (0..a.len()).step_by(width) {
        let v = get_raw(a, width, pos) as i64 + get_raw(b, width, pos) as i64;
        put_raw(&mut out, width, v.clamp(lo, hi) as i32);
    }
    out
}

/// In-place [`add`] of `b` onto `a[offset..]`. Same arithmetic; used where
/// the pydub code builds a new buffer that would be byte-identical anyway.
pub fn add_into(a: &mut [u8], b: &[u8], width: usize) {
    assert!(a.len() >= b.len());
    let (lo, hi) = int_bounds(width);
    match width {
        2 => {
            for (x, y) in a.chunks_exact_mut(2).zip(b.chunks_exact(2)) {
                let v = i16::from_le_bytes([x[0], x[1]]) as i32
                    + i16::from_le_bytes([y[0], y[1]]) as i32;
                x.copy_from_slice(&(v.clamp(lo as i32, hi as i32) as i16).to_le_bytes());
            }
        }
        _ => {
            for pos in (0..b.len()).step_by(width) {
                let v = get_raw(a, width, pos) as i64 + get_raw(b, width, pos) as i64;
                let mut buf = Vec::with_capacity(width);
                put_raw(&mut buf, width, v.clamp(lo, hi) as i32);
                a[pos..pos + width].copy_from_slice(&buf);
            }
        }
    }
}

/// `audioop.tostereo(fragment, width, lfactor, rfactor)`.
pub fn tostereo(data: &[u8], width: usize, lfactor: f64, rfactor: f64) -> Vec<u8> {
    check_len(data.len(), width);
    let (lo, hi) = bounds(width);
    let mut out = Vec::with_capacity(data.len() * 2);
    for pos in (0..data.len()).step_by(width) {
        let val = get_raw(data, width, pos) as f64;
        put_raw(
            &mut out,
            width,
            fbound(val * lfactor, lo, hi).floor() as i32,
        );
        put_raw(
            &mut out,
            width,
            fbound(val * rfactor, lo, hi).floor() as i32,
        );
    }
    out
}

/// `audioop.tomono(fragment, width, lfactor, rfactor)`.
pub fn tomono(data: &[u8], width: usize, lfactor: f64, rfactor: f64) -> Vec<u8> {
    check_len(data.len(), width * 2);
    let (lo, hi) = bounds(width);
    let mut out = Vec::with_capacity(data.len() / 2);
    for pos in (0..data.len()).step_by(width * 2) {
        let v1 = get_raw(data, width, pos) as f64;
        let v2 = get_raw(data, width, pos + width) as f64;
        let val = v1 * lfactor + v2 * rfactor;
        put_raw(&mut out, width, fbound(val, lo, hi).floor() as i32);
    }
    out
}

/// `audioop.lin2lin(fragment, width, newwidth)`.
pub fn lin2lin(data: &[u8], width: usize, newwidth: usize) -> Vec<u8> {
    check_len(data.len(), width);
    if width == newwidth {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len() / width * newwidth);
    for pos in (0..data.len()).step_by(width) {
        put_sample32(&mut out, newwidth, get_sample32(data, width, pos));
    }
    out
}

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b > 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// `audioop.ratecv(fragment, width, nchannels, inrate, outrate, None)` with
/// the default weights (1, 0). Only the converted bytes are returned; pydub
/// discards the state.
pub fn ratecv(data: &[u8], width: usize, nchannels: usize, inrate: u32, outrate: u32) -> Vec<u8> {
    let bytes_per_frame = width * nchannels;
    assert!(
        data.len() % bytes_per_frame == 0,
        "audioop.error: not a whole number of frames"
    );
    let (mut inrate, mut outrate) = (inrate as i64, outrate as i64);
    let d0 = gcd(inrate, outrate);
    inrate /= d0;
    outrate /= d0;
    // weightA = 1, weightB = 0 reduce to themselves.
    let (weight_a, weight_b) = (1.0f64, 0.0f64);

    let mut prev_i = vec![0i32; nchannels];
    let mut cur_i = vec![0i32; nchannels];
    let mut len = (data.len() / bytes_per_frame) as i64;
    let mut d = -outrate;
    let capacity = if len == 0 {
        0
    } else {
        ((1 + (len - 1) / inrate) * outrate) as usize * bytes_per_frame
    };
    let mut out = Vec::with_capacity(capacity);
    let mut cp = 0usize;

    loop {
        while d < 0 {
            if len == 0 {
                return out;
            }
            for chan in 0..nchannels {
                prev_i[chan] = cur_i[chan];
                cur_i[chan] = get_sample32(data, width, cp);
                cp += width;
                // The "simple digital filter"; exact for the default weights,
                // but kept in double arithmetic as the C code has it.
                cur_i[chan] = ((weight_a * cur_i[chan] as f64 + weight_b * prev_i[chan] as f64)
                    / (weight_a + weight_b)) as i32;
            }
            len -= 1;
            d += outrate;
        }
        while d >= 0 {
            for chan in 0..nchannels {
                let cur_o = ((prev_i[chan] as f64 * d as f64
                    + cur_i[chan] as f64 * (outrate - d) as f64)
                    / outrate as f64) as i32;
                put_sample32(&mut out, width, cur_o);
            }
            d -= inrate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[i16]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }
    fn u(b: &[u8]) -> Vec<i16> {
        b.chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    // Expected values below were printed by audioop-lts 0.2.2 on CPython 3.13.

    #[test]
    fn mul_floors_and_saturates() {
        assert_eq!(
            u(&mul(&s(&[3, -3, 1, -1, 32767, -32768, 100, -100]), 2, 0.5)),
            [1, -2, 0, -1, 16383, -16384, 50, -50]
        );
        assert_eq!(
            u(&mul(&s(&[3, -3, 32767, -32768, -21846, 21845]), 2, 1.5)),
            [4, -5, 32767, -32768, -32768, 32767]
        );
        assert_eq!(
            u(&mul(&s(&[-32767, -32768, 32767]), 2, 1.00001)),
            [-32768, -32768, 32767]
        );
        assert_eq!(u(&mul(&s(&[5, -5]), 2, -0.3)), [-2, 1]);
    }

    #[test]
    fn add_saturates() {
        assert_eq!(
            u(&add(
                &s(&[30000, -30000, 1, -32768]),
                &s(&[30000, -30000, -1, -1]),
                2
            )),
            [32767, -32768, 0, -32768]
        );
        let mut a = s(&[30000, -30000, 1, -32768, 7]);
        add_into(&mut a, &s(&[30000, -30000, -1, -1]), 2);
        assert_eq!(u(&a), [32767, -32768, 0, -32768, 7]);
    }

    #[test]
    fn channel_conversions_floor() {
        assert_eq!(
            u(&tostereo(&s(&[3, -3, 32767, -32768]), 2, 0.5, 1.0)),
            [1, 3, -2, -3, 16383, 32767, -16384, -32768]
        );
        assert_eq!(
            u(&tomono(
                &s(&[3, 4, -3, -4, 32767, 32767, -32768, -32768, 1, -2]),
                2,
                0.5,
                0.5
            )),
            [3, -4, 32767, -32768, -1]
        );
    }

    #[test]
    fn lin2lin_scales_by_shift() {
        assert!(lin2lin(&[], 1, 2).is_empty());
        assert_eq!(lin2lin(&[1, 255], 1, 2), vec![0x00, 0x01, 0x00, 0xff]);
    }

    fn ramp(n: usize) -> Vec<u8> {
        s(&(0..n)
            .map(|i| ((i * 1237) % 65536) as i64 - 32768)
            .map(|v| v as i16)
            .collect::<Vec<_>>())
    }

    #[test]
    fn ratecv_matches_cpython_lengths_and_values() {
        let cases: &[(usize, u32, u32, usize, &[i16])] = &[
            (
                1,
                11025,
                44100,
                145,
                &[
                    -32768, -32459, -32150, -31841, -31531, -31222, -30913, -30604, -30294, -29985,
                    -29676, -29367,
                ],
            ),
            (
                1,
                44100,
                48000,
                40,
                &[
                    -32768, -31632, -30496, -29359, -28223, -27086, -25950, -24813, -23677, -22540,
                    -21404, -20267,
                ],
            ),
            (
                2,
                44100,
                48000,
                80,
                &[
                    -32768, -31531, -30496, -29259, -28223, -26986, -25950, -24713, -23677, -22440,
                    -21404, -20167,
                ],
            ),
            (
                1,
                48000,
                44100,
                34,
                &[
                    -32768, -31422, -30076, -28729, -27383, -26037, -24690, -23344, -21997, -20651,
                    -19305, -17958,
                ],
            ),
            (
                2,
                22050,
                44100,
                146,
                &[
                    -32768, -31531, -31531, -30294, -30294, -29057, -29057, -27820, -27820, -26583,
                    -26583, -25346,
                ],
            ),
            (
                1,
                8000,
                44100,
                199,
                &[
                    -32768, -32544, -32320, -32095, -31871, -31647, -31422, -31198, -30973, -30749,
                    -30525, -30300,
                ],
            ),
        ];
        for &(ch, a, b, out_len, head) in cases {
            let out = u(&ratecv(&ramp(ch * 37), 2, ch, a, b));
            assert_eq!(out.len(), out_len, "{ch}ch {a}->{b}");
            assert_eq!(&out[..12], head, "{ch}ch {a}->{b}");
        }
        assert!(ratecv(&[], 2, 1, 11025, 44100).is_empty());
    }
}
