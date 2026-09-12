//! Audio engine: interleaved i16 PCM buffers with operations that reproduce
//! pydub/audioop bit for bit, an ffmpeg subprocess bridge for every codec
//! round trip, and ID3/WAV tagging.
//!
//! Phase 3 of the port fills in the PCM and ffmpeg halves. The tag reader
//! arrived early because `xil status` needs the SFX grade frame.

pub mod tags;
