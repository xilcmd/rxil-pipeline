//! An ID3v2.4 reader/writer that saves exactly what `mutagen` saves.
//!
//! The pipeline records SHA-256 digests of stems in manifests and logs, so
//! a tag that merely *reads* the same is not enough: the bytes must match.
//! This follows `mutagen.id3` 1.47's save path:
//!
//! * frames sorted by `(priority, len(frame data), HashKey)`, priority being
//!   the index in `TIT2 TPE1 TRCK TALB TPOS TDRC TCON`, then everything else,
//!   then `APIC` (whose relative order is kept);
//! * every encoded string followed by its terminator, text frames with an
//!   empty `"\0".join(text)` dropped entirely;
//! * padding from `PaddingInfo.get_default_padding` — keep the space already
//!   there unless it exceeds 10 KiB + 1 % of the file, otherwise 1 KiB +
//!   0.1 % — with the old tag region resized in place.
//!
//! Frames this module does not model are carried over byte for byte.

use std::fs;
use std::io;
use std::path::Path;

use indexmap::IndexMap;

const ORDER: [&str; 7] = ["TIT2", "TPE1", "TRCK", "TALB", "TPOS", "TDRC", "TCON"];

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// `T???` text frames (and `TDRC`-style timestamps).
    Text { enc: u8, values: Vec<String> },
    Txxx {
        enc: u8,
        desc: String,
        values: Vec<String>,
    },
    Comm {
        enc: u8,
        lang: [u8; 3],
        desc: String,
        values: Vec<String>,
    },
    Uslt {
        enc: u8,
        lang: [u8; 3],
        desc: String,
        text: String,
    },
    Apic {
        enc: u8,
        mime: String,
        ptype: u8,
        desc: String,
        data: Vec<u8>,
    },
    /// Anything else, written back unchanged.
    Raw(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub id: String,
    pub body: Body,
}

impl Frame {
    pub fn text(id: &str, value: &str) -> Frame {
        Frame {
            id: id.into(),
            body: Body::Text {
                enc: 3,
                values: vec![value.into()],
            },
        }
    }

    /// mutagen's `HashKey`: the key under which `tags.add` replaces a frame.
    pub fn hash_key(&self) -> String {
        match &self.body {
            Body::Txxx { desc, .. } => format!("TXXX:{desc}"),
            Body::Comm { desc, lang, .. } => {
                format!("COMM:{desc}:{}", String::from_utf8_lossy(lang))
            }
            Body::Uslt { desc, lang, .. } => {
                format!("USLT:{desc}:{}", String::from_utf8_lossy(lang))
            }
            Body::Apic { desc, .. } => format!("APIC:{desc}"),
            _ => self.id.clone(),
        }
    }
}

/// A loaded (or fresh) tag, frames keyed and ordered as mutagen's dict.
#[derive(Clone, Debug, Default)]
pub struct Tag {
    pub frames: IndexMap<String, Frame>,
}

fn syncsafe(n: usize) -> [u8; 4] {
    [
        ((n >> 21) & 0x7f) as u8,
        ((n >> 14) & 0x7f) as u8,
        ((n >> 7) & 0x7f) as u8,
        (n & 0x7f) as u8,
    ]
}

fn read_syncsafe(b: &[u8]) -> usize {
    b.iter()
        .fold(0usize, |acc, &x| (acc << 7) | (x & 0x7f) as usize)
}

fn encode(enc: u8, s: &str) -> Vec<u8> {
    match enc {
        0 => s
            .chars()
            .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
            .collect(),
        1 => {
            let mut v = vec![0xff, 0xfe];
            v.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
            v
        }
        2 => s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect(),
        _ => s.as_bytes().to_vec(),
    }
}

fn term(enc: u8) -> &'static [u8] {
    if enc == 1 || enc == 2 {
        &[0, 0]
    } else {
        &[0]
    }
}

fn encoded_text(enc: u8, s: &str) -> Vec<u8> {
    let mut v = encode(enc, s);
    v.extend_from_slice(term(enc));
    v
}

/// Split one terminated string off the front of `data`.
fn read_encoded(enc: u8, data: &[u8]) -> (String, usize) {
    let t = term(enc);
    let step = t.len();
    let mut i = 0;
    let end = loop {
        if i + step > data.len() {
            break None;
        }
        if &data[i..i + step] == t {
            break Some(i);
        }
        i += step;
    };
    let (raw, consumed) = match end {
        Some(e) => (&data[..e], e + step),
        None => (data, data.len()),
    };
    let s = match enc {
        0 => raw.iter().map(|&b| b as char).collect(),
        1 => {
            let (le, body) = match raw {
                [0xfe, 0xff, rest @ ..] => (false, rest),
                [0xff, 0xfe, rest @ ..] => (true, rest),
                rest => (true, rest),
            };
            let units: Vec<u16> = body
                .chunks_exact(2)
                .map(|c| {
                    if le {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                })
                .collect();
            String::from_utf16_lossy(&units)
        }
        2 => {
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(raw).into_owned(),
    };
    (s, consumed)
}

fn read_multi(enc: u8, mut data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let (s, n) = read_encoded(enc, data);
        out.push(s);
        data = &data[n..];
    }
    out
}

fn parse_body(id: &str, data: &[u8]) -> Body {
    let raw = || Body::Raw(data.to_vec());
    if data.is_empty() {
        return raw();
    }
    let enc = data[0];
    if enc > 3 {
        return raw();
    }
    let rest = &data[1..];
    match id {
        "TXXX" => {
            let (desc, n) = read_encoded(enc, rest);
            Body::Txxx {
                enc,
                desc,
                values: read_multi(enc, &rest[n..]),
            }
        }
        "COMM" | "USLT" if rest.len() >= 3 => {
            let lang = [rest[0], rest[1], rest[2]];
            let (desc, n) = read_encoded(enc, &rest[3..]);
            let after = &rest[3 + n..];
            if id == "COMM" {
                Body::Comm {
                    enc,
                    lang,
                    desc,
                    values: read_multi(enc, after),
                }
            } else {
                Body::Uslt {
                    enc,
                    lang,
                    desc,
                    text: read_encoded(enc, after).0,
                }
            }
        }
        "APIC" => {
            let Some(mend) = rest.iter().position(|&b| b == 0) else {
                return raw();
            };
            let mime: String = rest[..mend].iter().map(|&b| b as char).collect();
            let after = &rest[mend + 1..];
            if after.is_empty() {
                return raw();
            }
            let ptype = after[0];
            let (desc, n) = read_encoded(enc, &after[1..]);
            Body::Apic {
                enc,
                mime,
                ptype,
                desc,
                data: after[1 + n..].to_vec(),
            }
        }
        t if t.starts_with('T') => Body::Text {
            enc,
            values: read_multi(enc, rest),
        },
        _ => raw(),
    }
}

fn write_body(f: &Frame) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    match &f.body {
        Body::Text { enc, values } => {
            if values.join("\0").is_empty() {
                return None;
            }
            v.push(*enc);
            for s in values {
                let s = if f.id == "TDRC"
                    || f.id == "TDOR"
                    || f.id == "TDRL"
                    || f.id == "TDTG"
                    || f.id == "TDEN"
                {
                    s.replace(' ', "T")
                } else {
                    s.clone()
                };
                v.extend(encoded_text(*enc, &s));
            }
        }
        Body::Txxx { enc, desc, values } => {
            v.push(*enc);
            v.extend(encoded_text(*enc, desc));
            for s in values {
                v.extend(encoded_text(*enc, s));
            }
        }
        Body::Comm {
            enc,
            lang,
            desc,
            values,
        } => {
            v.push(*enc);
            v.extend_from_slice(lang);
            v.extend(encoded_text(*enc, desc));
            for s in values {
                v.extend(encoded_text(*enc, s));
            }
        }
        Body::Uslt {
            enc,
            lang,
            desc,
            text,
        } => {
            v.push(*enc);
            v.extend_from_slice(lang);
            v.extend(encoded_text(*enc, desc));
            v.extend(encoded_text(*enc, text));
        }
        Body::Apic {
            enc,
            mime,
            ptype,
            desc,
            data,
        } => {
            v.push(*enc);
            v.extend(mime.bytes());
            v.push(0);
            v.push(*ptype);
            v.extend(encoded_text(*enc, desc));
            v.extend_from_slice(data);
        }
        Body::Raw(data) => v.extend_from_slice(data),
    }
    Some(v)
}

/// The size of an existing ID3v2 tag at the start of `data` (header
/// included), or 0.
fn existing_tag_size(data: &[u8]) -> usize {
    if data.len() < 10 || &data[..3] != b"ID3" {
        return 0;
    }
    let flags = data[5];
    let footer = if data[3] == 4 && flags & 0x10 != 0 {
        10
    } else {
        0
    };
    read_syncsafe(&data[6..10]) + 10 + footer
}

impl Tag {
    /// `ID3(path)` — `None` when the file has no ID3v2 header
    /// (`ID3NoHeaderError`).
    pub fn load(data: &[u8]) -> Option<Tag> {
        let size = existing_tag_size(data);
        if size == 0 {
            return None;
        }
        let version = data[3];
        let flags = data[5];
        let end = (size.min(data.len())).max(10);
        let mut pos = 10;
        if flags & 0x40 != 0 && pos + 4 <= end {
            // Extended header.
            let ext = if version == 4 {
                read_syncsafe(&data[pos..pos + 4])
            } else {
                u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                    as usize
                    + 4
            };
            pos += ext;
        }
        let mut tag = Tag::default();
        while pos + 10 <= end {
            let id = &data[pos..pos + 4];
            if id[0] == 0 {
                break;
            }
            let fsize = if version == 4 {
                read_syncsafe(&data[pos + 4..pos + 8])
            } else {
                u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize
            };
            let body_start = pos + 10;
            let body_end = (body_start + fsize).min(end);
            let id = String::from_utf8_lossy(id).into_owned();
            let frame = Frame {
                id: id.clone(),
                body: parse_body(&id, &data[body_start..body_end]),
            };
            tag.add(frame);
            pos = body_start + fsize;
        }
        Some(tag)
    }

    /// `tags.add(frame)` — replaces a frame with the same HashKey in place.
    pub fn add(&mut self, frame: Frame) {
        self.frames.insert(frame.hash_key(), frame);
    }

    pub fn get(&self, key: &str) -> Option<&Frame> {
        self.frames.get(key)
    }

    /// `ID3._write`: the frame bytes in mutagen's order.
    fn frame_data(&self) -> Vec<u8> {
        let mut items: Vec<(usize, &Frame, Vec<u8>)> = Vec::new();
        for (i, f) in self.frames.values().enumerate() {
            let Some(body) = write_body(f) else { continue };
            let mut data = Vec::with_capacity(body.len() + 10);
            data.extend_from_slice(&f.id.as_bytes()[..4.min(f.id.len())]);
            data.extend_from_slice(&syncsafe(body.len()));
            data.extend_from_slice(&[0, 0]);
            data.extend(body);
            items.push((i, f, data));
        }
        let prio = |f: &Frame| match ORDER.iter().position(|o| *o == f.id) {
            Some(p) => p,
            None if f.id == "APIC" => ORDER.len() + 1,
            None => ORDER.len(),
        };
        items.sort_by(|a, b| {
            let ka = (
                prio(a.1),
                if a.1.id == "APIC" { a.0 } else { a.2.len() },
                a.1.hash_key(),
            );
            let kb = (
                prio(b.1),
                if b.1.id == "APIC" { b.0 } else { b.2.len() },
                b.1.hash_key(),
            );
            ka.cmp(&kb)
        });
        items.into_iter().flat_map(|(_, _, d)| d).collect()
    }

    /// `tags.save(path)` with mutagen's defaults (v2.4, default padding).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let file = fs::read(path).unwrap_or_default();
        let old_size = existing_tag_size(&file).min(file.len());
        let frames = self.frame_data();
        let needed = frames.len() + 10;
        let trailing = file.len() as i64;
        let available = old_size as i64 - needed as i64;
        let high = 1024 * 10 + trailing / 100;
        let low = 1024 + trailing / 1000;
        let padding = if available >= 0 {
            if available > high {
                low
            } else {
                available
            }
        } else {
            low
        } as usize;
        let new_size = needed + padding;
        let mut out = Vec::with_capacity(new_size + file.len() - old_size);
        out.extend_from_slice(b"ID3");
        out.extend_from_slice(&[4, 0, 0]);
        out.extend_from_slice(&syncsafe(new_size - 10));
        out.extend(frames);
        out.resize(new_size, 0);
        out.extend_from_slice(&file[old_size..]);
        fs::write(path, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syncsafe_round_trip() {
        assert_eq!(syncsafe(1024), [0, 0, 8, 0]);
        assert_eq!(read_syncsafe(&syncsafe(123_456)), 123_456);
    }

    #[test]
    fn text_frames_round_trip_and_sort() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.mp3");
        fs::write(&p, b"\xff\xfb\x90\x00audio").unwrap();
        let mut t = Tag::default();
        t.add(Frame::text("TCON", "Podcast"));
        t.add(Frame::text("TALB", "Show"));
        t.add(Frame::text("TIT2", "Title"));
        t.add(Frame::text("TPE1", ""));
        t.save(&p).unwrap();
        let data = fs::read(&p).unwrap();
        let loaded = Tag::load(&data).unwrap();
        let ids: Vec<&str> = loaded.frames.values().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids,
            ["TIT2", "TALB", "TCON"],
            "empty TPE1 dropped, priority order"
        );
        assert!(data.ends_with(b"\xff\xfb\x90\x00audio"));
        // 1 KiB + 0.1 % of the 9-byte file.
        assert_eq!(existing_tag_size(&data), 10 + 17 + 16 + 19 + 1024);
    }
}
