//! VDM download engine. Has no UI; the GUI and CLI drive it.
//!
//! [`Downloader`] probes a URL, then downloads it over several connections
//! with dynamic segment splitting when the server supports ranges (or over a
//! single stream when it doesn't), resuming from a `.vdmpart` file, with
//! retries and backoff.
//!
//! [`Engine`] runs a persistent queue of downloads on top of it, with speed
//! limits ([`RateLimiter`]) and a daily time window ([`Schedule`]).

mod auth;
pub mod checksum;
mod download;
pub mod engine;
mod error;
pub mod filename;
mod ftp;
mod ftp_download;
mod http;
pub mod limiter;
mod media;
mod part;
mod probe;
mod pwrite;
pub mod schedule;
mod segment;
mod segmented;
pub mod torrent;

pub use auth::{ProxySetting, SiteLogin, login_for, url_host};
pub use checksum::Checksum;
pub use download::{
    Completed, DownloadRequest, Downloader, MAX_CONNECTIONS, Observer, Options, Progress,
    RetryPolicy, Started,
};
pub use engine::{Engine, EngineConfig, EngineError};
pub use error::{Error, Result};
pub use limiter::RateLimiter;
pub use media::is_playlist_type;
pub use media::{MediaInfo, MediaKind};
pub use probe::ProbeInfo;
pub use schedule::Schedule;
pub use tokio_util::sync::CancellationToken;
pub use torrent::{Seed, is_torrent_url};
pub use vdm_media::{Quality, Variant, is_media_url};
pub use vdm_proto as proto;

/// Suffix of in-progress files.
pub const PART_SUFFIX: &str = part::PART_SUFFIX;
