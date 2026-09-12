//! Audio engine: interleaved i16 PCM buffers with operations that
//! reproduce pydub/audioop bit for bit, an ffmpeg subprocess bridge for
//! every codec round trip, and ID3/WAV tagging.

pub mod ffmpeg;
pub mod mpeg;
pub mod pcm;
pub mod tags;
