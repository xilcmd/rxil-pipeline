//! Client side of the newline-delimited JSON protocol spoken by the three
//! Python ML workers (chatterbox_turbo_worker.py, whisper_worker.py,
//! mmaudio_worker.py). Those workers stay Python, in their own venvs.
//!
//! Phase 5 of the port fills this crate in. It mirrors `_WorkerClient` in
//! `sfx_backends.py`.
