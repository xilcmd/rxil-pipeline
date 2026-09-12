//! Shared core for the xil pipeline: workspace layout, slug/path derivation,
//! config models, the SFX edit journal, and logging.
//!
//! Mirrors `models.py` and `log_config.py` from the Python package. Where
//! the Python does something odd (resolving symlinks, creating an empty log
//! file at startup), this crate does the same odd thing on purpose — the
//! parity harness compares the two byte for byte.

pub mod banner;
pub mod fsutil;
pub mod journal;
pub mod log;
pub mod pycsv;
pub mod pyjson;
pub mod script;
pub mod sfxlib;
pub mod stems;
pub mod textsim;
pub mod workspace;
