//! HTTP test server for exercising the VDM engine.
//!
//! Serves in-memory files with single-range `Range` support, `If-Range`,
//! `ETag`/`Last-Modified`/`Content-Disposition`, and optional faults:
//! ignoring ranges, cutting responses short (all of them, or every N-th one),
//! failing the first N requests, capping concurrent bodies, throttling, and
//! hiding `Content-Length`. It also reports how many bodies are streaming at once.
//!
//! ```no_run
//! # async fn demo() -> std::io::Result<()> {
//! use vdm_testsrv::{Faults, TestFile, TestServer, pattern_bytes};
//! let srv = TestServer::start(
//!     [("/a.bin", TestFile::new(pattern_bytes(1 << 20, 1)))],
//!     Faults::default(),
//! ).await?;
//! let url = srv.url("/a.bin");
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::stream;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Size of each body chunk the server writes.
const CHUNK: usize = 16 * 1024;

/// One file served by the test server.
#[derive(Debug, Clone)]
pub struct TestFile {
    pub data: Bytes,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_disposition: Option<String>,
}

impl TestFile {
    /// A file with a strong ETag derived from its length and a fixed Last-Modified.
    pub fn new(data: impl Into<Bytes>) -> Self {
        let data = data.into();
        Self {
            etag: Some(format!("\"len-{}\"", data.len())),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
            content_disposition: None,
            data,
        }
    }

    pub fn with_etag(mut self, etag: impl Into<String>) -> Self {
        self.etag = Some(etag.into());
        self
    }

    pub fn with_content_disposition(mut self, value: impl Into<String>) -> Self {
        self.content_disposition = Some(value.into());
        self
    }
}

/// Faults applied to every response.
#[derive(Debug, Clone, Default)]
pub struct Faults {
    /// Ignore `Range`, always answer `200` and omit `Accept-Ranges`.
    pub no_ranges: bool,
    /// Abort each response body after this many bytes.
    ///
    /// With [`cut_every`](Self::cut_every) set, only every n-th body is cut.
    pub cut_after: Option<u64>,
    /// Apply `cut_after` only to every n-th GET body response: the n-th, 2n-th, ...
    /// (counting `200`/`206` GET responses from 1). Other responses are served in full.
    /// `None` applies `cut_after` to every response. `Some(0)` behaves like `Some(1)`.
    pub cut_every: Option<u64>,
    /// Answer the first N requests with `503` and `Retry-After: 1`.
    pub fail_first: u64,
    /// Per-response throughput limit in bytes per second.
    pub throttle_bps: Option<u64>,
    /// Omit `Content-Length` (forces chunked transfer).
    pub hide_length: bool,
    /// Refuse a GET that would stream a body while this many bodies are already
    /// streaming: it gets `503` and `Retry-After: 1` instead. HEAD, `404` and `416`
    /// responses are never refused, and refused requests are not counted as in flight.
    pub max_concurrent: Option<usize>,
}

#[derive(Debug, Default)]
struct Stats {
    requests: AtomicU64,
    range_headers: Mutex<Vec<Option<String>>>,
    /// All request headers, in arrival order.
    headers: Mutex<Vec<HeaderMap>>,
    /// GET bodies (`200`/`206`) currently being streamed.
    in_flight: AtomicUsize,
    /// Highest value `in_flight` has reached.
    max_in_flight: AtomicUsize,
    /// GET `200`/`206` responses that have been admitted so far, for `cut_every`.
    bodies: AtomicU64,
}

#[derive(Debug)]
struct Shared {
    files: RwLock<HashMap<String, TestFile>>,
    faults: RwLock<Faults>,
    stats: Stats,
}

/// A running test server. It stops when dropped.
#[derive(Debug)]
pub struct TestServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl TestServer {
    /// Start on a random local port.
    pub async fn start<P: Into<String>>(
        files: impl IntoIterator<Item = (P, TestFile)>,
        faults: Faults,
    ) -> io::Result<Self> {
        Self::start_on(SocketAddr::from(([127, 0, 0, 1], 0)), files, faults).await
    }

    /// Start on a specific address.
    pub async fn start_on<P: Into<String>>(
        addr: SocketAddr,
        files: impl IntoIterator<Item = (P, TestFile)>,
        faults: Faults,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            files: RwLock::new(files.into_iter().map(|(p, f)| (p.into(), f)).collect()),
            faults: RwLock::new(faults),
            stats: Stats::default(),
        });
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let app = Router::new().fallback(serve).with_state(shared.clone());
        let task = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::error!(%err, "test server stopped");
            }
        });
        Ok(Self { addr, shared, task })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Full URL for a served path, e.g. `url("/a.bin")`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Add or replace a file, e.g. to simulate the file changing mid-download.
    pub fn set_file(&self, path: impl Into<String>, file: TestFile) {
        write_lock(&self.shared.files).insert(path.into(), file);
    }

    /// Replace the active faults.
    pub fn set_faults(&self, faults: Faults) {
        *write_lock(&self.shared.faults) = faults;
    }

    /// Number of requests received so far (including failed ones).
    pub fn requests(&self) -> u64 {
        self.shared.stats.requests.load(Ordering::Relaxed)
    }

    /// The `Range` header of every request, in arrival order.
    /// The value of header `name` in every request, in arrival order.
    pub fn header_values(&self, name: &str) -> Vec<Option<String>> {
        lock(&self.shared.stats.headers)
            .iter()
            .map(|h| h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned))
            .collect()
    }

    pub fn range_headers(&self) -> Vec<Option<String>> {
        lock(&self.shared.stats.range_headers).clone()
    }

    /// Number of GET bodies being streamed right now.
    pub fn in_flight(&self) -> usize {
        self.shared.stats.in_flight.load(Ordering::Acquire)
    }

    /// Highest number of GET bodies that were streamed at the same time so far.
    pub fn max_in_flight(&self) -> usize {
        self.shared.stats.max_in_flight.load(Ordering::Acquire)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Deterministic pseudo-random content, so downloads can be checked byte for byte.
pub fn pattern_bytes(len: usize, seed: u64) -> Bytes {
    // xorshift64*; seed 0 would get stuck at zero.
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        out.extend_from_slice(&state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out.into()
}

/// Result of interpreting a `Range` header against a file length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeRequest {
    /// Serve the whole file with `200`.
    Full,
    /// Serve `start..=end` with `206`.
    Partial { start: u64, end: u64 },
    /// Answer `416`.
    Unsatisfiable,
}

/// Interpret a single-range `Range` header (RFC 9110 §14.2).
///
/// Malformed headers and multi-range requests fall back to [`RangeRequest::Full`],
/// which the RFC allows.
pub fn parse_range(value: &str, len: u64) -> RangeRequest {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeRequest::Full;
    };
    if spec.contains(',') {
        return RangeRequest::Full;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return RangeRequest::Full;
    };
    match (first.trim(), last.trim()) {
        ("", "") => RangeRequest::Full,
        ("", suffix) => match suffix.parse::<u64>() {
            Ok(0) => RangeRequest::Unsatisfiable,
            Ok(_) if len == 0 => RangeRequest::Unsatisfiable,
            Ok(n) => RangeRequest::Partial { start: len.saturating_sub(n), end: len - 1 },
            Err(_) => RangeRequest::Full,
        },
        (first, last) => {
            let Ok(start) = first.parse::<u64>() else {
                return RangeRequest::Full;
            };
            let end = if last.is_empty() {
                None
            } else {
                match last.parse::<u64>() {
                    Ok(end) if end >= start => Some(end),
                    _ => return RangeRequest::Full,
                }
            };
            if start >= len {
                return RangeRequest::Unsatisfiable;
            }
            let end = end.map_or(len - 1, |e| e.min(len - 1));
            RangeRequest::Partial { start, end }
        }
    }
}

async fn serve(
    State(shared): State<Arc<Shared>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let n = shared.stats.requests.fetch_add(1, Ordering::Relaxed);
    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    lock(&shared.stats.range_headers).push(range_header.clone());
    lock(&shared.stats.headers).push(headers.clone());

    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let faults = read_lock(&shared.faults).clone();
    if n < faults.fail_first {
        return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "1")]).into_response();
    }

    let Some(file) = read_lock(&shared.files).get(uri.path()).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = file.data.len() as u64;

    let mut range = match &range_header {
        Some(v) if !faults.no_ranges => parse_range(v, len),
        _ => RangeRequest::Full,
    };
    if let (RangeRequest::Partial { .. }, Some(if_range)) = (range, headers.get(header::IF_RANGE)) {
        let if_range = if_range.to_str().unwrap_or_default();
        let matches = file.etag.as_deref() == Some(if_range)
            || file.last_modified.as_deref() == Some(if_range);
        if !matches {
            range = RangeRequest::Full;
        }
    }

    let mut resp_headers = HeaderMap::new();
    let mut set = |name: header::HeaderName, value: &str| {
        if let Ok(v) = HeaderValue::from_str(value) {
            resp_headers.insert(name, v);
        }
    };
    set(header::CONTENT_TYPE, "application/octet-stream");
    if !faults.no_ranges {
        set(header::ACCEPT_RANGES, "bytes");
    }
    if let Some(v) = &file.etag {
        set(header::ETAG, v);
    }
    if let Some(v) = &file.last_modified {
        set(header::LAST_MODIFIED, v);
    }
    if let Some(v) = &file.content_disposition {
        set(header::CONTENT_DISPOSITION, v);
    }

    let (status, start, end_excl) = match range {
        RangeRequest::Full => (StatusCode::OK, 0, len),
        RangeRequest::Partial { start, end } => {
            set(header::CONTENT_RANGE, &format!("bytes {start}-{end}/{len}"));
            (StatusCode::PARTIAL_CONTENT, start, end + 1)
        }
        RangeRequest::Unsatisfiable => {
            set(header::CONTENT_RANGE, &format!("bytes */{len}"));
            return (StatusCode::RANGE_NOT_SATISFIABLE, resp_headers).into_response();
        }
    };
    if !faults.hide_length {
        set(header::CONTENT_LENGTH, &(end_excl - start).to_string());
    }

    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        let Some(guard) = admit_body(&shared, faults.max_concurrent) else {
            return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "1")]).into_response();
        };
        // Numbered only after admission: refused requests are not `200`/`206` responses.
        let seq = shared.stats.bodies.fetch_add(1, Ordering::Relaxed) + 1;
        let cut_after = match faults.cut_every {
            Some(n) if seq % n.max(1) != 0 => None,
            _ => faults.cut_after,
        };
        let slice = file.data.slice(start as usize..end_excl as usize);
        Body::from_stream(body_stream(slice, cut_after, faults.throttle_bps, guard))
    };
    (status, resp_headers, body).into_response()
}

/// One streaming body's slot in [`Stats::in_flight`]. Released on drop, so the count
/// falls whether the body finished or the client went away.
struct InFlight(Arc<Shared>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.stats.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Claim a body slot, or `None` when `limit` bodies are already in flight.
///
/// The check and the increment are one compare-exchange, so two requests racing
/// for the last slot cannot both get it.
fn admit_body(shared: &Arc<Shared>, limit: Option<usize>) -> Option<InFlight> {
    let counter = &shared.stats.in_flight;
    let mut current = counter.load(Ordering::Acquire);
    loop {
        if limit.is_some_and(|k| current >= k) {
            return None;
        }
        match counter.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
    shared.stats.max_in_flight.fetch_max(current + 1, Ordering::AcqRel);
    Some(InFlight(shared.clone()))
}

/// Stream `data` in chunks, optionally throttled, optionally aborting after `cut_after` bytes.
///
/// `guard` is part of the stream state, so the in-flight slot is held until the
/// stream ends or is dropped.
fn body_stream(
    data: Bytes,
    cut_after: Option<u64>,
    throttle_bps: Option<u64>,
    guard: InFlight,
) -> impl futures_util::Stream<Item = io::Result<Bytes>> + Send + 'static {
    let limit = cut_after.map_or(data.len(), |c| (c as usize).min(data.len()));
    let cut = limit < data.len();
    stream::unfold((data, 0usize, false, guard), move |(data, sent, done, guard)| async move {
        if done {
            return None;
        }
        if sent >= limit {
            if !cut {
                return None;
            }
            // Pause so hyper flushes the headers and bytes sent so far; otherwise the
            // client sees no response at all instead of a body cut mid-stream.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let err = io::Error::new(io::ErrorKind::ConnectionAborted, "fault: cut_after");
            return Some((Err(err), (data, sent, true, guard)));
        }
        let n = CHUNK.min(limit - sent);
        if let Some(bps) = throttle_bps.filter(|&b| b > 0) {
            tokio::time::sleep(Duration::from_secs_f64(n as f64 / bps as f64)).await;
        }
        let chunk = data.slice(sent..sent + n);
        Some((Ok(chunk), (data, sent + n, false, guard)))
    })
}

// Lock helpers that ignore poisoning: a panicking test thread must not wedge the server.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
fn read_lock<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}
fn write_lock<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use RangeRequest::*;

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=0-99", 1000), Partial { start: 0, end: 99 });
        assert_eq!(parse_range("bytes=500-", 1000), Partial { start: 500, end: 999 });
        assert_eq!(parse_range("bytes=900-5000", 1000), Partial { start: 900, end: 999 });
        assert_eq!(parse_range("bytes=-100", 1000), Partial { start: 900, end: 999 });
        assert_eq!(parse_range("bytes=-5000", 1000), Partial { start: 0, end: 999 });
        assert_eq!(parse_range("bytes=1000-", 1000), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 1000), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-0", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=5-1", 1000), Full);
        assert_eq!(parse_range("bytes=0-1,5-9", 1000), Full);
        assert_eq!(parse_range("items=0-1", 1000), Full);
        assert_eq!(parse_range("bytes=abc", 1000), Full);
    }

    #[test]
    fn pattern_is_deterministic() {
        assert_eq!(pattern_bytes(1000, 3), pattern_bytes(1000, 3));
        assert_ne!(pattern_bytes(1000, 3), pattern_bytes(1000, 4));
        assert_eq!(pattern_bytes(13, 0).len(), 13);
    }
}
