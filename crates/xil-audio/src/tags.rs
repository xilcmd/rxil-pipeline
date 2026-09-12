//! ID3 tag access. Mirrors the mutagen calls in `sfx_common.py`.

use std::path::Path;

/// The user-text frame that carries an SFX quality grade.
pub const SFX_GRADE_FRAME: &str = "XIL_GRADE";
pub const SFX_GRADE_ACCURATE: &str = "accurate";
pub const SFX_GRADE_REJECTED: &str = "rejected";

/// Value of the `TXXX` frame whose description is `desc`, if present.
/// Multi-value frames yield their first value, like mutagen's `text[0]`.
pub fn read_txxx(path: &Path, desc: &str) -> Option<String> {
    let tag = id3::Tag::read_from_path(path).ok()?;
    let value = tag
        .extended_texts()
        .find(|t| t.description == desc)
        .map(|t| t.value.split('\0').next().unwrap_or("").to_string());
    value
}

/// `'accurate'` / `'rejected'` from the file's grade frame, else `""`. Never
/// fails: a missing file, missing header, or unknown value all read as ungraded.
pub fn read_sfx_grade(path: &Path) -> String {
    match read_txxx(path, SFX_GRADE_FRAME) {
        Some(v) => {
            let v = v.trim().to_lowercase();
            if v == SFX_GRADE_ACCURATE || v == SFX_GRADE_REJECTED {
                v
            } else {
                String::new()
            }
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use id3::TagLike;

    #[test]
    fn grade_round_trips_through_txxx() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.mp3");
        std::fs::write(&p, b"").unwrap();
        let mut tag = id3::Tag::new();
        tag.add_frame(id3::frame::ExtendedText {
            description: SFX_GRADE_FRAME.into(),
            value: " Rejected ".into(),
        });
        tag.write_to_path(&p, id3::Version::Id3v24).unwrap();
        assert_eq!(read_sfx_grade(&p), "rejected");
        assert_eq!(read_sfx_grade(tmp.path().join("missing.mp3").as_path()), "");
    }

    #[test]
    fn unknown_grade_reads_as_ungraded() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("y.mp3");
        std::fs::write(&p, b"").unwrap();
        let mut tag = id3::Tag::new();
        tag.add_frame(id3::frame::ExtendedText {
            description: SFX_GRADE_FRAME.into(),
            value: "meh".into(),
        });
        tag.write_to_path(&p, id3::Version::Id3v24).unwrap();
        assert_eq!(read_sfx_grade(&p), "");
    }
}
