//! Find out what a URL points at before downloading it.

use reqwest::header::{self, HeaderMap};
use reqwest::{Client, StatusCode, Url};

use crate::error::{Error, Result};
use crate::filename;
use crate::http::{self, ContentRange};

/// What the server told us about a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeInfo {
    /// URL after redirects.
    pub final_url: String,
    /// Total size, when the server reports one.
    pub size: Option<u64>,
    /// The server honors byte ranges and reports a size, so the download can resume.
    pub resumable: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    /// Sanitized name from `Content-Disposition` or the URL.
    pub file_name: String,
}

/// One probe request: `GET` with `Range: bytes=0-0`. More servers handle this
/// correctly than `HEAD`, and a `206` reply proves that ranges work.
pub(crate) async fn probe_once(
    client: &Client,
    url: &Url,
    headers: &HeaderMap,
) -> Result<ProbeInfo> {
    let resp = client
        .get(url.clone())
        .headers(headers.clone())
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(Error::Network)?;
    let status = resp.status();
    let h = resp.headers();

    let (size, resumable) = match status {
        StatusCode::PARTIAL_CONTENT => match http::content_range(h) {
            Some(ContentRange::Bytes { start: 0, total, .. }) => (total, total.is_some()),
            other => {
                return Err(Error::Protocol(format!("probe got 206 with Content-Range {other:?}")));
            }
        },
        // Server ignored the range: no resume. Dropping `resp` closes the connection
        // without reading the body.
        StatusCode::OK => (http::content_length(h), false),
        // `bytes=0-0` is only unsatisfiable for an empty file.
        StatusCode::RANGE_NOT_SATISFIABLE => match http::content_range(h) {
            Some(ContentRange::Unsatisfied { total: 0 }) => (Some(0), true),
            other => {
                return Err(Error::Protocol(format!("probe got 416 with Content-Range {other:?}")));
            }
        },
        _ => return Err(Error::Status { status, retry_after: http::retry_after(h) }),
    };

    let final_url = resp.url().clone();
    let file_name = filename::choose(http::header_str(h, header::CONTENT_DISPOSITION), &final_url);
    Ok(ProbeInfo {
        final_url: final_url.into(),
        size,
        resumable,
        etag: http::header_string(h, header::ETAG),
        last_modified: http::header_string(h, header::LAST_MODIFIED),
        content_type: http::header_string(h, header::CONTENT_TYPE),
        file_name,
    })
}
