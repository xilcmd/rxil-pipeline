//! Client side of the newline-delimited JSON protocol spoken by the Python
//! ML workers (`chatterbox_turbo_worker.py`, `whisper_worker.py`,
//! `mmaudio_worker.py`). The workers stay Python, in their own venvs; Rust
//! only starts them, writes one JSON request per line and reads one JSON
//! response per line.
//!
//! Startup differs by caller, so both reads are exposed: `stem-verify`
//! parses the very first line as the ready message, while the SFX and TTS
//! clients skip model-loading noise until a `{"ready": true}` line appears.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::Value;
use xil_core::pyjson::{dumps, Style};

/// A running worker process.
pub struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    /// `subprocess.Popen([python, script, *args], stdin=PIPE, stdout=PIPE)`
    /// with stderr inherited, as every Python caller leaves it.
    pub fn spawn(python: &Path, script: &Path, args: &[String]) -> io::Result<Worker> {
        let mut child = Command::new(python)
            .arg(script)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Worker {
            child,
            stdin,
            stdout,
        })
    }

    /// `proc.stdout.readline()`: the next line including its newline, or
    /// an empty string at end of stream.
    pub fn read_line(&mut self) -> io::Result<String> {
        let mut buf = Vec::new();
        self.stdout.read_until(b'\n', &mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Read lines until one parses as JSON with a truthy `ready`, returning
    /// it. Non-JSON lines and other messages go to `on_noise`. `None` means
    /// the worker exited first.
    pub fn wait_ready(&mut self, mut on_noise: impl FnMut(&str)) -> io::Result<Option<Value>> {
        loop {
            let raw = self.read_line()?;
            if raw.is_empty() {
                return Ok(None);
            }
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(msg) if is_truthy(msg.get("ready")) => return Ok(Some(msg)),
                _ => on_noise(line),
            }
        }
    }

    /// Write `json.dumps(request) + "\n"` and flush.
    pub fn send(&mut self, request: &Value) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker stdin closed"))?;
        let mut line = dumps(request, Style::COMPACT);
        line.push('\n');
        stdin.write_all(line.as_bytes())?;
        stdin.flush()
    }

    /// Close stdin (the worker's loop ends) and wait for it to exit.
    pub fn close(mut self) -> io::Result<()> {
        drop(self.stdin.take());
        self.child.wait().map(|_| ())
    }
}

/// Python truthiness for the `ready` field.
pub fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

/// `<package_dir>/<name>` — where a worker script lives next to the Python
/// package that ships it.
pub fn worker_script(package_dir: &Path, name: &str) -> PathBuf {
    package_dir.join(name)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A shell "python" that ignores the script argument, prints noise and
    /// a ready line, then echoes each request's `n` back doubled.
    fn fake_python(dir: &Path) -> PathBuf {
        let p = dir.join("fake-python");
        std::fs::write(
            &p,
            "#!/bin/sh\necho 'loading weights...'\necho '{\"ready\": true, \"device\": \"cpu\"}'\nwhile read line; do echo '{\"done\": true}'; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn handshake_request_close() {
        let tmp = tempfile::tempdir().unwrap();
        let py = fake_python(tmp.path());
        let mut w = Worker::spawn(&py, Path::new("worker.py"), &["cpu".into()]).unwrap();
        let mut noise = Vec::new();
        let ready = w
            .wait_ready(|l| noise.push(l.to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(ready["device"], "cpu");
        assert_eq!(noise, vec!["loading weights..."]);
        w.send(&serde_json::json!({"prompt": "door"})).unwrap();
        let resp: Value = serde_json::from_str(&w.read_line().unwrap()).unwrap();
        assert_eq!(resp["done"], true);
        w.close().unwrap();
    }
}
