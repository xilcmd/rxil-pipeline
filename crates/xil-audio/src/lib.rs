//! Audio engine: interleaved i16 PCM buffers with operations that reproduce
//! pydub/audioop bit for bit, an ffmpeg subprocess bridge for every codec
//! round trip, and ID3/WAV tagging.
//!
//! Phase 3 of the port fills this crate in. It mirrors the pydub surface used
//! by `mix_common.py`, the ffmpeg pipe in `audio_fx.py`, and the mutagen
//! tagging in `sfx_common.py`.
