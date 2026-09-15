//! Dialogue TTS backends shared by `sample` and `produce`: the Chatterbox
//! Turbo worker bridge and the gTTS draft backend. Ports of `_ChatterboxClient`
//! and `_gtts_generate` (the two copies in the Python are identical).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::bail;
use regex::Regex;
use serde_json::{Map, Value};
use xil_audio::fx::py_repr;
use xil_core::fsutil::basename;
use xil_core::log;
use xil_workers::Worker;

const LABEL: &str = "Chatterbox Turbo";

/// The two Python copies of `_ChatterboxClient` differ in two details.
#[derive(Clone, Copy, PartialEq)]
pub enum Flavor {
    /// `XILU004`: reads exactly one line per response.
    Sample,
    /// `XILP002`: skips non-JSON noise between responses, and names the
    /// venv in its startup failure.
    Produce,
}

/// `_ChatterboxClient`: starts `chatterbox_turbo_worker.py` on first use.
pub struct Chatterbox {
    python: String,
    voice_refs_dir: String,
    device: String,
    flavor: Flavor,
    worker: Option<Worker>,
}

impl Chatterbox {
    pub fn new(python: &str, voice_refs_dir: &str, device: &str, flavor: Flavor) -> Chatterbox {
        Chatterbox {
            python: python.to_string(),
            voice_refs_dir: voice_refs_dir.to_string(),
            device: device.to_string(),
            flavor,
            worker: None,
        }
    }

    fn start(&mut self) -> anyhow::Result<()> {
        let script = crate::workers::python_package_dir()
            .map(|d| xil_workers::worker_script(&d, "chatterbox_turbo_worker.py"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot locate the xil_pipeline package that ships chatterbox_turbo_worker.py"
                )
            })?;
        log::info(&format!(
            "Starting {LABEL} worker ({}, {})…",
            self.python, self.device
        ));
        let mut w = Worker::spawn(
            Path::new(&self.python),
            &script,
            std::slice::from_ref(&self.device),
        )?;
        let Some(msg) =
            w.wait_ready(|line| log::debug(&format!("{LABEL} worker startup: {line}")))?
        else {
            if self.flavor == Flavor::Produce {
                bail!("RuntimeError: {LABEL} worker exited before sending ready signal. Check that venv-chatterbox is correctly set up and the model is downloaded.");
            }
            bail!("RuntimeError: {LABEL} worker exited before sending ready signal.");
        };
        let actual = match msg.get("device") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => self.device.clone(),
        };
        if actual != self.device {
            log::warning(&format!(
                "{LABEL}: requested device {} unavailable, running on {}",
                py_repr(&self.device),
                py_repr(&actual)
            ));
        }
        let Some(sr) = msg.get("sr").and_then(Value::as_i64) else {
            bail!("KeyError: 'sr'");
        };
        log::info(&format!(
            "{LABEL} worker ready (sample_rate={sr}, device={actual})"
        ));
        self.worker = Some(w);
        Ok(())
    }

    fn ref_for(&self, speaker: &str) -> Option<PathBuf> {
        [".wav", ".mp3"]
            .iter()
            .map(|ext| Path::new(&self.voice_refs_dir).join(format!("{speaker}{ext}")))
            .find(|p| p.exists())
    }

    /// `generate(text, out_path, speaker_key)`.
    pub fn generate(&mut self, text: &str, out_path: &Path, speaker: &str) -> anyhow::Result<()> {
        if self.worker.is_none() {
            self.start()?;
        }
        let r = self.ref_for(speaker);
        if let Some(r) = &r {
            log::info(&format!("   ref: {}", basename(r)));
        }
        let mut req = Map::new();
        req.insert("text".into(), text.into());
        req.insert(
            "out_path".into(),
            out_path.to_string_lossy().into_owned().into(),
        );
        req.insert(
            "ref_audio".into(),
            r.map(|p| Value::from(p.to_string_lossy().into_owned()))
                .unwrap_or(Value::Null),
        );
        req.insert(
            "cond_path".into(),
            Path::new(&self.voice_refs_dir)
                .join(format!("{speaker}.turbo.conds.pt"))
                .to_string_lossy()
                .into_owned()
                .into(),
        );
        let flavor = self.flavor;
        let w = self.worker.as_mut().expect("started");
        w.send(&Value::Object(req))?;
        let resp: Value = loop {
            let raw = w.read_line()?;
            if raw.is_empty() {
                bail!("RuntimeError: {LABEL} worker closed pipe unexpectedly.");
            }
            if flavor == Flavor::Sample {
                break serde_json::from_str(&raw)?;
            }
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(v) => break v,
                Err(_) => log::debug(&format!("{LABEL} worker: {line}")),
            }
        };
        if let Some(err) = resp.get("error") {
            let msg = match err {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            bail!("RuntimeError: {LABEL}: {msg}");
        }
        Ok(())
    }

    pub fn close(&mut self) {
        if let Some(w) = self.worker.take() {
            let _ = w.close();
        }
    }
}

/// `_gtts_generate(text, out_path)`: strip `[tags]`, synthesize, atomic write.
/// The producer's copy also logs the request.
pub fn gtts_generate(text: &str, out_path: &Path, flavor: Flavor) -> anyhow::Result<()> {
    static TAGS: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\[[^\]]*\]").unwrap());
    let cleaned = TAGS.replace_all(text, "").trim().to_string();
    if cleaned.is_empty() {
        return Ok(());
    }
    let dir = out_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix("tmp")
        .rand_bytes(8)
        .suffix(".tmp")
        .tempfile_in(dir)?;
    if flavor == Flavor::Produce {
        log::info(&format!(
            "   > gTTS ({} chars) → {}",
            cleaned.chars().count(),
            basename(out_path)
        ));
    }
    let audio = xil_api::gtts::synthesize(&cleaned, "en")
        .map_err(|e| anyhow::anyhow!("gtts.tts.gTTSError: {e}"))?;
    fs::write(tmp.path(), audio)?;
    tmp.persist(out_path)
        .map_err(|e| anyhow::anyhow!(e.error))?;
    Ok(())
}
