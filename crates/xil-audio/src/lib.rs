//! Audio engine: interleaved i16 PCM buffers with operations that
//! reproduce pydub/audioop bit for bit, an ffmpeg subprocess bridge for
//! every codec round trip, and ID3/WAV tagging.

pub mod audioop;
pub mod ffmpeg;
pub mod fx;
pub mod id3w;
pub mod mpeg;
pub mod pcm;
pub mod segment;
pub mod tags;
