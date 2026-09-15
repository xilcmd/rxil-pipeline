//! The optional session activity log (`xil gui --output FILE`).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

static LOG: Mutex<Option<File>> = Mutex::new(None);

/// Open (append) the activity log. Later calls replace the file.
pub fn open(path: &Path) -> std::io::Result<()> {
    let f = OpenOptions::new().append(true).create(true).open(path)?;
    if let Ok(mut g) = LOG.lock() {
        *g = Some(f);
    }
    Ok(())
}

/// `_log_activity(msg)`: one `[YYYY-MM-DDTHH:MM:SS] msg` line, local time.
/// A no-op when no log is open.
pub fn log(msg: &str) {
    if let Ok(mut g) = LOG.lock() {
        if let Some(f) = g.as_mut() {
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
            let _ = writeln!(f, "[{ts}] {msg}");
            let _ = f.flush();
        }
    }
}
