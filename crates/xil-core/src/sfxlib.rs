//! Shared SFX library naming — the text half of `sfx_common.py`.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

/// Filesystem max is 255 bytes; leave room for `.mp3` + collision suffix.
const MAX_SLUG_LEN: usize = 180;

static NON_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9_]+").unwrap());
static MULTI_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-{2,}").unwrap());

/// Direction text → filesystem-safe slug.
///
/// Lowercase; `": "` → `_`; any other run of non `[a-z0-9_]` → `-`; collapse
/// dashes; strip leading/trailing dashes; over 180 chars, truncate and append
/// `_` + 8 hex chars of the full slug's SHA-256.
pub fn slugify_effect_key(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let slug = text.to_lowercase().replace(": ", "_");
    let slug = NON_SLUG.replace_all(&slug, "-");
    let slug = MULTI_DASH.replace_all(&slug, "-");
    let slug = slug.trim_matches('-').to_string();
    if slug.chars().count() > MAX_SLUG_LEN {
        let h = format!("{:x}", Sha256::digest(slug.as_bytes()));
        let head: String = slug.chars().take(MAX_SLUG_LEN).collect();
        return format!("{}_{}", head.trim_end_matches('-'), &h[..8]);
    }
    slug
}

/// Shared library path for an effect key. The default backend keeps the
/// plain historical filename; any other backend gets a `.<backend>` infix.
pub fn shared_sfx_path(sfx_dir: &Path, effect_key: &str, backend: &str) -> PathBuf {
    let slug = slugify_effect_key(effect_key);
    let suffix = if backend == "elevenlabs" {
        String::new()
    } else {
        format!(".{backend}")
    };
    sfx_dir.join(format!("{slug}{suffix}.mp3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_examples_from_python_docstring() {
        assert_eq!(slugify_effect_key("BEAT"), "beat");
        assert_eq!(
            slugify_effect_key("SFX: DOOR OPENS, BELL CHIMES"),
            "sfx_door-opens-bell-chimes"
        );
        assert_eq!(
            slugify_effect_key("AMBIENCE: Café — night"),
            "ambience_caf-night"
        );
        assert_eq!(slugify_effect_key(""), "");
        assert_eq!(slugify_effect_key("--x--"), "x");
    }

    #[test]
    fn long_keys_get_hash_suffix() {
        let long = "a".repeat(200);
        let s = slugify_effect_key(&long);
        assert_eq!(s.len(), 180 + 1 + 8);
        assert!(s.starts_with(&"a".repeat(180)));
        assert!(s[181..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn shared_path_backend_infix() {
        assert_eq!(
            shared_sfx_path(Path::new("SFX"), "BEAT", "elevenlabs"),
            Path::new("SFX/beat.mp3")
        );
        assert_eq!(
            shared_sfx_path(Path::new("SFX"), "SFX: X", "audioldm2"),
            Path::new("SFX/sfx_x.audioldm2.mp3")
        );
    }
}
