//! HLS and DASH for VDM, without any networking.
//!
//! [`hls`] parses M3U8 playlists and [`dash`] parses MPD manifests. Both turn
//! a stream into [`Track`]s: lists of [`Segment`]s (URL, byte range,
//! encryption) that `vdm-core` fetches, decrypts and joins into one file.
//! [`Quality`] picks among the [`Variant`]s a stream offers.

pub mod dash;
pub mod hls;
mod plan;

pub use plan::{
    Container, Encryption, Quality, Segment, Track, TrackKind, Variant, choose, is_media_url,
};
pub use url::Url;

/// A playlist or manifest that can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

impl ParseError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}
