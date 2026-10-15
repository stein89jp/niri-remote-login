//! Optional host NVENC encoding; IronRDP owns graphics negotiation and framing.
//!
//! `encoder` wraps the native FFmpeg/NVENC context, `gfx` provides the
//! per-connection graphics pipeline factory and display wrapper, and `updates`
//! is the `VideoUpdates` stream that turns desktop captures into H.264 frames.
mod encoder;
mod gfx;
mod updates;

pub use encoder::{Encoder, h264_has_idr};
pub use gfx::{AudioFeedback, Video};

#[cfg(test)]
mod tests;
