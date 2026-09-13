//! Every `Segment` operation the mixer uses, checked against pydub 0.25.1
//! with audioop-lts 0.2.2 on CPython 3.13.
//!
//! The table was produced by running the same operations on the same
//! synthetic signals in Python and hashing the raw PCM. Several rows are
//! pydub quirks rather than sensible results — a fade longer than its clip
//! *grows* the clip (`B.fade_out150`, `L.fade_out3000`) — and they are here
//! precisely because a DAW layer is only byte-identical if those survive.
//!
//! Linux only: gains, fades and pans go through the C library's `pow` and
//! `log`, exactly as Python's do, and the hashes were recorded against
//! glibc. macOS and Windows round a few of those calls differently — so
//! does Python there, which keeps the port faithful but the table glibc's.
#![cfg(target_os = "linux")]

use sha2::{Digest, Sha256};
use xil_audio::segment::Segment;

/// `int(20000*sin(i*k)) + (i*7919) % 601 - 300`, clamped to 16 bits.
fn gen(frames: usize, rate: u32, ch: usize, k: f64) -> Segment {
    let data = (0..frames * ch)
        .map(|i| {
            let v = (20000.0 * (i as f64 * k).sin()) as i64 + ((i as i64 * 7919) % 601) - 300;
            v.clamp(-32768, 32767) as i16
        })
        .flat_map(|v| v.to_le_bytes())
        .collect();
    Segment::new(data, 2, rate, ch)
}

fn vintage(s: &Segment) -> Segment {
    let mono = s.clone().set_channels(1).set_channels(s.channels);
    mono.low_pass_filter(5000.0)
        .high_pass_filter(150.0)
        .gain(-3.0)
}

const GOLDEN: &[(&str, usize, u32, usize, usize, &str)] = &[
    ("A", 2, 22050, 1, 1554, "0122fd3ac43a7209"),
    ("B", 2, 44100, 2, 4936, "282720d93081a736"),
    ("A.gain-3.7", 2, 22050, 1, 1554, "9fa45c9359727ba7"),
    ("B.gain+2.2", 2, 44100, 2, 4936, "a221319cc4635ba6"),
    ("A.fade_in50", 2, 22050, 1, 1554, "966451a96d96e1a6"),
    ("A.fade_in250", 2, 22050, 1, 1586, "1d046c2d32358400"),
    ("B.fade_out20", 2, 44100, 2, 4936, "3d81a70eee0c58ed"),
    ("B.fade_out150", 2, 44100, 2, 14456, "8440c5427b2a89ab"),
    ("L.fade_in300", 2, 44100, 2, 405720, "e83dfe3b90a9da65"),
    ("L.fade_out1200", 2, 44100, 2, 405720, "d5a20570098a4c9e"),
    ("L.fade_in90", 2, 44100, 2, 405720, "3c4e011aca053de3"),
    ("L.fade_out3000", 2, 44100, 2, 811260, "ae4b96ef1a248c74"),
    ("A.pan-0.3", 2, 22050, 2, 3108, "9a78b0c183b0069c"),
    ("A.pan0", 2, 22050, 2, 3108, "325ce3dbf3760fad"),
    ("B.pan1", 2, 44100, 2, 4936, "805cd32339fcbfd8"),
    ("B.pan0.45", 2, 44100, 2, 4936, "bcd3e4e1e00b12f0"),
    ("A.rate44100", 2, 44100, 1, 3106, "b28b0cc76b0fb13e"),
    ("B.rate48000", 2, 48000, 2, 5372, "d39c558fdb5e8905"),
    ("M.rate11025", 2, 11025, 1, 18376, "1aa88ef3e9b2bcfb"),
    ("B.mono", 2, 44100, 1, 2468, "0088d151b39836ec"),
    ("A.stereo", 2, 22050, 2, 3108, "2400008f0a19ec31"),
    ("silent.overlayA", 2, 22050, 1, 88200, "abbbf9ec1bdc4ebc"),
    ("silent.overlayB", 2, 44100, 2, 529376, "0d7aa1576fd6ab00"),
    ("chain", 2, 44100, 2, 475632, "0464d9464efa56bf"),
    ("A[:13]", 2, 22050, 1, 572, "b01ba3db414f893d"),
    ("A[5:]", 2, 22050, 1, 1322, "2f4c1c3e69a2bd42"),
    ("M[:pct]", 2, 24000, 1, 14976, "9a6565be2e8db8c1"),
    ("B.lpf", 2, 44100, 2, 4936, "b0b55e00ca131237"),
    ("B.hpf", 2, 44100, 2, 4936, "5ed07749b034320d"),
    ("M.vintage", 2, 24000, 1, 40000, "368fe63c5d991ed7"),
    ("B.vintage", 2, 44100, 2, 4936, "afa9dd548601eb5b"),
    ("A*3[:100]", 2, 22050, 1, 4410, "8b9de1375f62aa0d"),
    ("loopM", 2, 24000, 1, 112560, "afc26f627725ead6"),
];

#[test]
fn every_operation_matches_pydub() {
    let a = gen(777, 22050, 1, 0.031);
    let b = gen(1234, 44100, 2, 0.017);
    let l = gen(101430, 44100, 2, 0.0021);
    let m = gen(20000, 24000, 1, 0.05);
    let m_len = m.len_ms();

    let run = |name: &str| -> Segment {
        match name {
            "A" => a.clone(),
            "B" => b.clone(),
            "A.gain-3.7" => a.gain(-3.7),
            "B.gain+2.2" => b.gain(2.2),
            "A.fade_in50" => a.fade_in(50.0),
            "A.fade_in250" => a.fade_in(250.0),
            "B.fade_out20" => b.fade_out(20.0),
            "B.fade_out150" => b.fade_out(150.0),
            "L.fade_in300" => l.fade_in(300.0),
            "L.fade_out1200" => l.fade_out(1200.0),
            "L.fade_in90" => l.fade_in(90.0),
            "L.fade_out3000" => l.fade_out(3000.0),
            "A.pan-0.3" => a.pan(-0.3),
            "A.pan0" => a.pan(0.0),
            "B.pan1" => b.pan(1.0),
            "B.pan0.45" => b.pan(0.45),
            "A.rate44100" => a.clone().set_frame_rate(44100),
            "B.rate48000" => b.clone().set_frame_rate(48000),
            "M.rate11025" => m.clone().set_frame_rate(11025),
            "B.mono" => b.clone().set_channels(1),
            "A.stereo" => a.clone().set_channels(2),
            "silent.overlayA" => Segment::silent(2000.0).overlay(a.clone(), 1000),
            "silent.overlayB" => Segment::silent(3001.0)
                .overlay(b.clone(), 17)
                .overlay(m.clone(), 2500),
            "chain" => Segment::empty()
                .append(a.clone())
                .append(Segment::silent(600.0))
                .append(b.clone())
                .append(Segment::silent(600.0))
                .append(m.clone())
                .append(Segment::silent(600.0)),
            "A[:13]" => a.slice(None, Some(13.0)),
            "A[5:]" => a.slice(Some(5.0), None),
            "M[:pct]" => m.slice(
                None,
                Some(((m_len as f64 * 37.5 / 100.0) as i64).max(1) as f64),
            ),
            "B.lpf" => b.low_pass_filter(5000.0),
            "B.hpf" => b.high_pass_filter(150.0),
            "M.vintage" => vintage(&m),
            "B.vintage" => vintage(&b),
            "A*3[:100]" => a.repeat(3).slice(None, Some(100.0)),
            "loopM" => {
                let repeats = -((-2345i64).div_euclid(m_len));
                m.repeat(repeats).slice(None, Some(2345.0))
            }
            other => panic!("unknown op {other}"),
        }
    };

    let mut failures = Vec::new();
    for &(name, width, rate, ch, nbytes, hash) in GOLDEN {
        let s = run(name);
        let got = format!("{:x}", Sha256::digest(&s.data));
        let got = &got[..16];
        if (s.sample_width, s.frame_rate, s.channels, s.data.len()) != (width, rate, ch, nbytes)
            || got != hash
        {
            failures.push(format!(
                "{name}: got ({}, {}, {}, {} bytes, {got}) want ({width}, {rate}, {ch}, {nbytes} bytes, {hash})",
                s.sample_width, s.frame_rate, s.channels, s.data.len()
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
