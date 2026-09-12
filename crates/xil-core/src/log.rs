//! Two sinks, two formats — port of `log_config.py`.
//!
//! Console (stdout) is human-readable: `INFO` and `RUN` bare, `[!] ` for
//! warnings, `[ERROR] `, `[CRITICAL] `, `[debug] `.
//!
//! The file `logs/xil_v2_<date>_<host>.log` under the workspace root is one
//! record per line: `<iso ts>|<LEVEL>|<host>|<stage>|<message>`. Multi-line
//! messages get the prefix on every physical line; whitespace-only records
//! are dropped from the file. The message may itself contain `|`.
//!
//! Python opens the file handler at configure time, which creates an empty
//! log file even when nothing is logged. [`init`] does the same, so the set
//! of files a command leaves behind matches.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

use chrono::Local;

use crate::workspace::workspace_root;

/// Severity, ordered like Python's numeric levels (`RUN` = 25 sits between
/// INFO and WARNING).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Debug = 10,
    Info = 20,
    Run = 25,
    Warning = 30,
    Error = 40,
    Critical = 50,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Run => "RUN",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
            Level::Critical => "CRITICAL",
        }
    }

    fn console_prefix(self) -> &'static str {
        match self {
            Level::Debug => "[debug] ",
            Level::Info | Level::Run => "",
            Level::Warning => "[!] ",
            Level::Error => "[ERROR] ",
            Level::Critical => "[CRITICAL] ",
        }
    }
}

/// Which sinks a record goes to. Mirrors `extra={"console": False}` /
/// `extra={"file": False}` in the Python.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sink {
    Both,
    ConsoleOnly,
    FileOnly,
}

struct Logger {
    stage: String,
    host: String,
    threshold: Level,
    file: Option<File>,
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

/// Short, filename-safe hostname: first label, non `[A-Za-z0-9._-]` → `-`.
pub fn host() -> String {
    let raw = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let first = raw.split('.').next().unwrap_or("unknown");
    let safe: String = first
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if safe.is_empty() {
        "unknown".to_string()
    } else {
        safe
    }
}

/// Path of today's log file for this host under `<workspace>/logs/`.
pub fn log_path() -> std::path::PathBuf {
    workspace_root().join("logs").join(format!(
        "xil_v2_{}_{}.log",
        Local::now().format("%Y-%m-%d"),
        host()
    ))
}

/// Install the logger for a command. `stage` is what Python derives from
/// `argv[0]` (`"use"`, `"parse"`, …). Safe to call more than once; only the
/// first call opens the file. Creates `logs/` and the (possibly empty) file.
pub fn init(stage: &str) {
    LOGGER.get_or_init(|| {
        let path = log_path();
        let file = path
            .parent()
            .and_then(|d| fs::create_dir_all(d).ok())
            .and_then(|_| {
                OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&path)
                    .ok()
            });
        Mutex::new(Logger {
            stage: stage.to_string(),
            host: host(),
            threshold: Level::Info,
            file,
        })
    });
}

/// Raise or lower the console/file threshold (Python `configure_logging(level)`).
pub fn set_threshold(level: Level) {
    if let Some(l) = LOGGER.get() {
        if let Ok(mut g) = l.lock() {
            g.threshold = level;
        }
    }
}

/// Emit one record. Callers normally use the level helpers below.
pub fn log(level: Level, sink: Sink, msg: &str) {
    let Some(l) = LOGGER.get() else {
        // Not initialised: behave like a bare print so nothing is lost.
        if level >= Level::Info {
            println!("{}{msg}", level.console_prefix());
        }
        return;
    };
    let Ok(mut g) = l.lock() else { return };
    if level < g.threshold {
        return;
    }
    if sink != Sink::FileOnly {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{}{msg}", level.console_prefix());
    }
    if sink != Sink::ConsoleOnly && !msg.trim().is_empty() {
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S%z");
        let prefix = format!("{ts}|{}|{}|{}|", level.name(), g.host, g.stage);
        let record: String = msg
            .split('\n')
            .map(|line| format!("{prefix}{line}\n"))
            .collect();
        if let Some(f) = g.file.as_mut() {
            let _ = f.write_all(record.as_bytes());
        }
    }
}

pub fn debug(msg: &str) {
    log(Level::Debug, Sink::Both, msg);
}
pub fn info(msg: &str) {
    log(Level::Info, Sink::Both, msg);
}
pub fn run(msg: &str) {
    log(Level::Run, Sink::Both, msg);
}
pub fn warning(msg: &str) {
    log(Level::Warning, Sink::Both, msg);
}
pub fn error(msg: &str) {
    log(Level::Error, Sink::Both, msg);
}
pub fn critical(msg: &str) {
    log(Level::Critical, Sink::Both, msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_order_like_python() {
        assert!(Level::Debug < Level::Info);
        assert!(Level::Info < Level::Run);
        assert!(Level::Run < Level::Warning);
        assert!(Level::Warning < Level::Error);
        assert!(Level::Error < Level::Critical);
    }

    #[test]
    fn console_prefixes_match_python() {
        assert_eq!(Level::Debug.console_prefix(), "[debug] ");
        assert_eq!(Level::Info.console_prefix(), "");
        assert_eq!(Level::Run.console_prefix(), "");
        assert_eq!(Level::Warning.console_prefix(), "[!] ");
        assert_eq!(Level::Error.console_prefix(), "[ERROR] ");
        assert_eq!(Level::Critical.console_prefix(), "[CRITICAL] ");
    }

    #[test]
    fn host_is_first_label_and_filename_safe() {
        let h = host();
        assert!(!h.is_empty());
        assert!(!h.contains('.'));
        assert!(h
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')));
    }
}
