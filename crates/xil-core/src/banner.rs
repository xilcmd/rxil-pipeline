//! Start/finish frame around a command run. Port of `sfx_common.run_banner`.
//!
//! The decorative bars are console-only; the `BEGIN`/`END` records are
//! file-only and give log parsers a per-invocation boundary.

use std::env;
use std::process;
use std::time::Instant;

use chrono::Local;

use crate::log::{self, Level, Sink};

const BAR: &str = "======================================================================";

/// An open banner. Dropping it writes the finish frame, so an early `?`
/// return still closes the run the way Python's `finally` does.
pub struct Banner {
    name: String,
    start: Instant,
}

/// Open the frame. `name` is what Python shows (`os.path.basename(argv[0])`
/// or an explicit script name); `argv_line` is `" ".join(sys.argv)`.
pub fn begin(name: &str, argv_line: &str) -> Banner {
    let now = Local::now();
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    log::log(Level::Run, Sink::ConsoleOnly, "");
    log::log(Level::Run, Sink::ConsoleOnly, BAR);
    log::log(
        Level::Run,
        Sink::ConsoleOnly,
        &format!("  {name}  |  started {}", now.format("%Y-%m-%d %H:%M:%S")),
    );
    log::log(Level::Run, Sink::ConsoleOnly, BAR);
    log::log(Level::Run, Sink::ConsoleOnly, "");
    log::log(
        Level::Run,
        Sink::FileOnly,
        &format!(
            "BEGIN argv=\"{argv_line}\" pid={} ver={} cwd={cwd}",
            process::id(),
            env!("CARGO_PKG_VERSION")
        ),
    );
    Banner {
        name: name.to_string(),
        start: Instant::now(),
    }
}

impl Drop for Banner {
    fn drop(&mut self) {
        let end = Local::now();
        let elapsed = self.start.elapsed().as_secs_f64();
        log::log(
            Level::Run,
            Sink::FileOnly,
            &format!("END elapsed={elapsed:.1}s"),
        );
        log::log(Level::Run, Sink::ConsoleOnly, "");
        log::log(Level::Run, Sink::ConsoleOnly, BAR);
        log::log(
            Level::Run,
            Sink::ConsoleOnly,
            &format!(
                "  {}  |  finished {}  ({elapsed:.1}s)",
                self.name,
                end.format("%Y-%m-%d %H:%M:%S")
            ),
        );
        log::log(Level::Run, Sink::ConsoleOnly, BAR);
        log::log(Level::Run, Sink::ConsoleOnly, "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_is_seventy_equals() {
        assert_eq!(BAR.len(), 70);
        assert!(BAR.chars().all(|c| c == '='));
    }
}
