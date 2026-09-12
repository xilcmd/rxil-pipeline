//! `os.path` / `glob` behaviours the commands lean on.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// `os.path.abspath`: absolute and lexically normalised (`.` and `..`
/// collapsed, trailing slash dropped). Symlinks are not resolved.
pub fn abspath(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `os.path.relpath(path, start)` for two absolute paths: `.` when equal,
/// `..` segments when `path` is not under `start`.
pub fn relpath(path: &Path, start: &Path) -> PathBuf {
    let a: Vec<_> = abspath(path)
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let b: Vec<_> = abspath(start)
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let common = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for seg in &a[common..] {
        out.push(seg);
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// `sorted(glob.glob(os.path.join(dir, f"{prefix}*{suffix}")))` — direct
/// children only, sorted by full path, dotfiles excluded like glob does.
pub fn glob_children(dir: &Path, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| name_matches(p, prefix, suffix))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// `sorted(glob.glob(str(root / "**" / f"{prefix}*{suffix}"), recursive=True))`
/// — every depth, sorted by full path, hidden directories and files skipped.
pub fn glob_recursive(root: &Path, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, prefix, suffix, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, prefix: &str, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        let hidden = p
            .file_name()
            .map(|n| n.to_string_lossy().starts_with('.'))
            .unwrap_or(false);
        if hidden {
            continue;
        }
        if p.is_dir() {
            walk(&p, prefix, suffix, out);
        } else if name_matches(&p, prefix, suffix) {
            out.push(p);
        }
    }
}

fn name_matches(p: &Path, prefix: &str, suffix: &str) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    !name.starts_with('.')
        && name.starts_with(prefix)
        && name.ends_with(suffix)
        && name.len() >= prefix.len() + suffix.len()
}

/// Directory entries in raw `readdir` order — what `os.listdir`, `os.scandir`
/// and `glob.glob` return. Some Python code never sorts, so neither can the port.
pub fn list_dir_raw(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// Sort paths the way Python sorts `Path` objects: by their string form.
/// `PathBuf`'s own `Ord` compares component-wise, which can differ when a
/// name contains a byte below `/`.
pub fn sort_py(paths: &mut [PathBuf]) {
    paths.sort_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
}

/// `sorted(Path(dir).glob(f"{prefix}*{suffix}"))` — pathlib's glob, which
/// unlike the `glob` module does match dotfiles.
pub fn pathlib_glob(dir: &Path, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = list_dir_raw(dir)
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    n.starts_with(prefix)
                        && n.ends_with(suffix)
                        && n.len() >= prefix.len() + suffix.len()
                })
                .unwrap_or(false)
        })
        .collect();
    sort_py(&mut out);
    out
}

/// `sorted(Path(dir).rglob(f"*{infix}*{suffix}"))` — every depth, dotfiles
/// and hidden directories included, filename must contain `infix` and end
/// with `suffix`.
pub fn pathlib_rglob_contains(dir: &Path, infix: &str, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for p in list_dir_raw(&d) {
            if p.is_dir() {
                stack.push(p);
            } else if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                if let Some(stem) = n.strip_suffix(suffix) {
                    if stem.contains(infix) {
                        out.push(p);
                    }
                }
            }
        }
    }
    sort_py(&mut out);
    out
}

/// `os.path.basename`
pub fn basename(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unix path semantics: on Windows "/a/b" is drive-relative, so
    // absolute() prepends a drive letter and the comparison is meaningless.
    // The pipeline only ever runs on Linux/WSL/macOS.
    #[test]
    #[cfg(unix)]
    fn abspath_normalises() {
        assert_eq!(abspath(Path::new("/a/b/../c/./d/")), Path::new("/a/c/d"));
        assert_eq!(abspath(Path::new("/a/b/")), Path::new("/a/b"));
    }

    #[test]
    fn relpath_cases() {
        assert_eq!(
            relpath(Path::new("/a/b/c.mp3"), Path::new("/a/b")),
            Path::new("c.mp3")
        );
        assert_eq!(
            relpath(Path::new("/a/b"), Path::new("/a/b")),
            Path::new(".")
        );
        assert_eq!(
            relpath(Path::new("/a/x/y"), Path::new("/a/b/c")),
            Path::new("../../x/y")
        );
    }

    #[test]
    fn globs_sort_and_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("sub/deeper")).unwrap();
        fs::create_dir_all(r.join(".hidden")).unwrap();
        for f in [
            "parsed_b.json",
            "parsed_a.json",
            "orig_parsed_a.json",
            "sub/parsed_c.json",
            "sub/deeper/parsed_d.json",
            ".hidden/parsed_z.json",
            "parsed_.json",
        ] {
            fs::write(r.join(f), "{}").unwrap();
        }
        let names = |v: Vec<PathBuf>| {
            v.iter()
                .map(|p| relpath(p, r).display().to_string().replace('\\', "/"))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(glob_children(r, "parsed_", ".json")),
            vec!["parsed_.json", "parsed_a.json", "parsed_b.json"]
        );
        assert_eq!(
            names(glob_recursive(r, "parsed_", ".json")),
            vec![
                "parsed_.json",
                "parsed_a.json",
                "parsed_b.json",
                "sub/deeper/parsed_d.json",
                "sub/parsed_c.json"
            ]
        );
    }
}
