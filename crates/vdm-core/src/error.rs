use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::StatusCode;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid URL `{0}`")]
    InvalidUrl(String),

    #[error("invalid request header `{0}`")]
    InvalidHeader(String),

    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    #[error("server returned {status}")]
    Status { status: StatusCode, retry_after: Option<Duration> },

    #[error("connection closed after {received} of {expected} bytes")]
    Incomplete { received: u64, expected: u64 },

    #[error("unexpected server response: {0}")]
    Protocol(String),

    #[error("the file changed on the server since the download started")]
    RemoteChanged,

    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cancelled after {downloaded} bytes")]
    Cancelled { downloaded: u64 },

    #[error("FTP: {message}")]
    Ftp { message: String, transient: bool },
}

impl Error {
    /// Whether retrying the same request may succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Network(e) => !e.is_builder() && !e.is_redirect(),
            Self::Status { status, .. } => {
                *status == StatusCode::REQUEST_TIMEOUT
                    || *status == StatusCode::TOO_MANY_REQUESTS
                    || status.is_server_error()
            }
            Self::Incomplete { .. } => true,
            Self::Ftp { transient, .. } => *transient,
            _ => false,
        }
    }

    /// Delay requested by the server (`Retry-After`), if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// Attach a path to I/O errors.
pub(crate) trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| Error::Io { path: path.to_owned(), source })
    }
}
