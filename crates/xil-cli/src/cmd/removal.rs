//! Shared pieces of `xil remove-show` and `xil remove-episode`: the removal
//! item model, size formatting, and deletion.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use xil_core::log;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    File,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: Kind,
    pub path: PathBuf,
    pub label: &'static str,
}

impl Item {
    pub fn dir(path: PathBuf, label: &'static str) -> Item {
        Item {
            kind: Kind::Dir,
            path,
            label,
        }
    }
    pub fn file(path: PathBuf, label: &'static str) -> Item {
        Item {
            kind: Kind::File,
            path,
            label,
        }
    }

    /// Files under a directory (`rglob("*")`, dotfiles included), or 1/0 for a file.
    pub fn file_count(&self) -> u64 {
        if !self.path.exists() {
            return 0;
        }
        match self.kind {
            Kind::File => 1,
            Kind::Dir => walk_files(&self.path).len() as u64,
        }
    }

    pub fn total_bytes(&self) -> u64 {
        if !self.path.exists() {
            return 0;
        }
        match self.kind {
            Kind::File => fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
            Kind::Dir => walk_files(&self.path)
                .iter()
                .filter_map(|p| fs::metadata(p).ok())
                .map(|m| m.len())
                .sum(),
        }
    }
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.filter_map(Result::ok) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// `_fmt_bytes`: B / KB / MB / GB with Python's `.1f` / `.2f` rounding.
pub fn fmt_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024u64.pow(2) {
        format!("{:.1} KB", n as f64 / KB)
    } else if n < 1024u64.pow(3) {
        format!("{:.1} MB", n as f64 / (KB * KB))
    } else {
        format!("{:.2} GB", n as f64 / (KB * KB * KB))
    }
}

/// `item.path.relative_to(root)` with the same fallback to the full path.
pub fn rel_display(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Delete every existing item, logging each removal. Returns files removed.
pub fn delete(items: &[Item]) -> anyhow::Result<u64> {
    let mut removed = 0;
    for item in items {
        if !item.path.exists() {
            continue;
        }
        match item.kind {
            Kind::Dir => {
                let fc = item.file_count();
                fs::remove_dir_all(&item.path)?;
                removed += fc;
            }
            Kind::File => {
                fs::remove_file(&item.path)?;
                removed += 1;
            }
        }
        log::info(&format!("  removed {}", item.path.display()));
    }
    Ok(removed)
}

/// Python `input(prompt).strip()`: prompt to stdout without a newline, one
/// line from stdin. Empty string on EOF.
pub fn input(prompt: &str) -> String {
    let mut so = io::stdout().lock();
    let _ = so.write_all(prompt.as_bytes());
    let _ = so.flush();
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    line.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_matches_python_rounding() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1280), "1.2 KB"); // 1.25 rounds half to even
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn counts_and_bytes_walk_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("d");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a"), "12345").unwrap();
        fs::write(d.join("sub/.hidden"), "123").unwrap();
        let item = Item::dir(d.clone(), "");
        assert_eq!(item.file_count(), 2);
        assert_eq!(item.total_bytes(), 8);
        let f = Item::file(d.join("a"), "");
        assert_eq!(f.file_count(), 1);
        assert_eq!(f.total_bytes(), 5);
        assert_eq!(Item::file(d.join("missing"), "").file_count(), 0);
    }
}
