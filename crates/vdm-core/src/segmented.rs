//! Multi-connection download of one file.
//!
//! Each worker is one connection: it claims a segment from the shared
//! [`SegmentTable`], fetches it with a closed `Range` request and writes it
//! in place with positional writes. When a worker runs out of idle segments,
//! `claim` splits the busiest one, so connections stay busy until the end.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{self, HeaderMap};
use reqwest::{Client, StatusCode, Url};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::download::{Observer, Progress, RetryPolicy};
use crate::error::{Error, IoContext, Result};
use crate::http::{self, ContentRange};
use crate::limiter::{RateLimiter, acquire_all};
use crate::part::{PartMeta, Slot};
use crate::pwrite::write_all_at;
use crate::segment::{SegmentId, SegmentTable};

/// How often progress is reported.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// How often the segment table is saved to the sidecar.
const SAVE_INTERVAL: Duration = Duration::from_secs(1);
/// Largest amount charged to the speed limiters at once.
const LIMIT_PIECE: usize = 16 * 1024;

/// Everything the workers share.
pub(crate) struct Ctx {
    pub client: Client,
    pub url: Url,
    pub headers: HeaderMap,
    /// Validators of the file version being downloaded.
    pub remote: PartMeta,
    pub table: SegmentTable,
    pub file: std::fs::File,
    pub part: PathBuf,
    pub retry: RetryPolicy,
    pub write_buffer: usize,
    /// Applied to every received chunk.
    pub limits: Vec<Arc<RateLimiter>>,
}

/// Run `workers` connections until the file is complete, the download is
/// cancelled, or a fatal error occurs. The segment table is saved to the
/// sidecar periodically and before returning, whatever the outcome.
pub(crate) async fn run<O>(
    ctx: Arc<Ctx>,
    workers: usize,
    slot: &Slot,
    meta: &mut PartMeta,
    observer: &mut O,
    cancel: &CancellationToken,
) -> Result<()>
where
    O: Observer,
{
    let stop = cancel.child_token();
    let mut set = JoinSet::new();
    for _ in 0..workers.max(1) {
        set.spawn(worker(ctx.clone(), stop.clone()));
    }

    let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_save = Instant::now();
    let mut saved = ctx.table.downloaded();
    let mut fatal: Option<Error> = None;
    let mut gave_up: Option<Error> = None;

    loop {
        tokio::select! {
            joined = set.join_next() => {
                let Some(joined) = joined else { break };
                let err = match joined {
                    Ok(Ok(()) | Err(Error::Cancelled { .. })) => continue,
                    Ok(Err(err)) => err,
                    Err(join) => Error::Io { path: ctx.part.clone(), source: std::io::Error::other(join) },
                };
                if err.is_transient() {
                    // This connection ran out of retries; the others carry on and
                    // will pick up its segment.
                    tracing::warn!(%err, "connection gave up");
                    gave_up = Some(err);
                } else if fatal.is_none() {
                    tracing::debug!(%err, "fatal error; stopping all connections");
                    stop.cancel();
                    fatal = Some(err);
                }
            }
            _ = tick.tick() => {
                let downloaded = ctx.table.downloaded();
                let connections = u8::try_from(ctx.table.active()).unwrap_or(u8::MAX);
                observer.segments(ctx.table.snapshot(), connections);
                observer.progress(Progress { downloaded, total: Some(ctx.table.total()) });
                if downloaded != saved && last_save.elapsed() >= SAVE_INTERVAL {
                    meta.segments = ctx.table.snapshot();
                    if let Err(err) = slot.write_meta(meta).await {
                        stop.cancel();
                        fatal.get_or_insert(err);
                    }
                    saved = downloaded;
                    last_save = Instant::now();
                }
            }
        }
    }

    meta.segments = ctx.table.snapshot();
    let save = slot.write_meta(meta).await;
    let downloaded = ctx.table.downloaded();
    observer.segments(ctx.table.snapshot(), 0);
    observer.progress(Progress { downloaded, total: Some(ctx.table.total()) });

    if let Some(err) = fatal {
        return Err(err);
    }
    save?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled { downloaded });
    }
    if ctx.table.is_complete() {
        return Ok(());
    }
    Err(gave_up.unwrap_or_else(|| {
        Error::Protocol("all connections stopped before the download finished".into())
    }))
}

/// One connection: claim segments and fetch them until there is nothing left.
async fn worker(ctx: Arc<Ctx>, cancel: CancellationToken) -> Result<()> {
    let mut failures = 0;
    loop {
        let Some(id) = ctx.table.claim() else { return Ok(()) };
        let (before, _) = ctx.table.range(id);
        let result = fetch(&ctx, id, &cancel).await;
        let (after, _) = ctx.table.range(id);
        ctx.table.release(id);

        let err = match result {
            Ok(()) => {
                failures = 0;
                continue;
            }
            Err(err) => err,
        };
        if matches!(err, Error::Cancelled { .. }) || !err.is_transient() {
            return Err(err);
        }
        if after > before {
            failures = 0;
        }
        failures += 1;
        if failures >= ctx.retry.max_attempts {
            return Err(err);
        }
        let delay = ctx.retry.delay(failures, err.retry_after());
        tracing::debug!(%err, failures, ?delay, "retrying segment");
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: 0 }),
        }
    }
}

/// Fetch the rest of segment `id`. Returns `Ok` once the segment is complete,
/// including when a split shortened it while we were downloading.
async fn fetch(ctx: &Arc<Ctx>, id: SegmentId, cancel: &CancellationToken) -> Result<()> {
    let (pos, end) = ctx.table.range(id);
    if pos >= end {
        return Ok(());
    }
    let mut req = ctx
        .client
        .get(ctx.url.clone())
        .headers(ctx.headers.clone())
        .header(header::RANGE, format!("bytes={pos}-{}", end - 1));
    if let Some(v) = ctx.remote.if_range() {
        req = req.header(header::IF_RANGE, v);
    }
    let mut resp = tokio::select! {
        r = req.send() => r.map_err(Error::Network)?,
        () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: 0 }),
    };

    let status = resp.status();
    let h = resp.headers();
    match status {
        StatusCode::PARTIAL_CONTENT => match http::content_range(h) {
            Some(ContentRange::Bytes { start, total, .. }) if start == pos => {
                if total.is_some_and(|t| t != ctx.table.total()) {
                    return Err(Error::RemoteChanged);
                }
            }
            other => {
                return Err(Error::Protocol(format!(
                    "asked for bytes {pos}-{}, got Content-Range {other:?}",
                    end - 1
                )));
            }
        },
        // A full body instead of our range: `If-Range` failed because the file
        // changed, or the server stopped honoring ranges.
        StatusCode::OK => {
            let seen = PartMeta::new(
                ctx.url.as_str(),
                http::content_length(h).or(Some(ctx.table.total())),
                http::header_string(h, header::ETAG),
                http::header_string(h, header::LAST_MODIFIED),
            );
            return Err(if seen.differs(&ctx.remote) {
                Error::RemoteChanged
            } else {
                Error::Protocol("server ignored the Range header".into())
            });
        }
        StatusCode::RANGE_NOT_SATISFIABLE => return Err(Error::RemoteChanged),
        _ => return Err(Error::Status { status, retry_after: http::retry_after(h) }),
    }

    let mut w = SegmentWriter {
        ctx: ctx.clone(),
        id,
        offset: pos,
        buf: Vec::with_capacity(ctx.write_buffer),
    };
    let streamed = stream_body(&mut resp, &mut w, cancel).await;
    // Bytes received before an error are valid; always keep them.
    w.flush().await?;
    let segment_done = streamed?;
    if !segment_done {
        let (pos, end) = ctx.table.range(id);
        if pos < end {
            return Err(Error::Incomplete { received: pos, expected: end });
        }
    }
    Ok(())
}

/// Copy the body into `w`. Returns `true` if the segment filled up (possibly
/// because a split shortened it), `false` if the body simply ended.
async fn stream_body(
    resp: &mut reqwest::Response,
    w: &mut SegmentWriter,
    cancel: &CancellationToken,
) -> Result<bool> {
    loop {
        let chunk = tokio::select! {
            c = resp.chunk() => c.map_err(Error::Network)?,
            () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: 0 }),
        };
        let Some(chunk) = chunk else { return Ok(false) };
        let mut data = &chunk[..];
        while !data.is_empty() {
            // Reserve before charging the speed limit, in small pieces: a chunk can
            // be large after a long limiter wait, and bytes past a split are dropped.
            let piece = data.len().min(LIMIT_PIECE);
            let allowed = w.ctx.table.reserve(w.id, piece as u64) as usize;
            if allowed == 0 {
                return Ok(true);
            }
            if !w.ctx.limits.is_empty() {
                tokio::select! {
                    () = acquire_all(&w.ctx.limits, allowed as u64) => {}
                    () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: 0 }),
                }
            }
            w.buf.extend_from_slice(&data[..allowed]);
            data = &data[allowed..];
            if w.buf.len() >= w.ctx.write_buffer {
                w.flush().await?;
            }
        }
    }
}

/// Buffers a worker's bytes and writes them at the segment's position.
struct SegmentWriter {
    ctx: Arc<Ctx>,
    id: SegmentId,
    /// File offset of the first buffered byte.
    offset: u64,
    buf: Vec<u8>,
}

impl SegmentWriter {
    async fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let buf = std::mem::take(&mut self.buf);
        let (offset, ctx) = (self.offset, self.ctx.clone());
        let (written, mut buf) = tokio::task::spawn_blocking(move || {
            let r = write_all_at(&ctx.file, &buf, offset);
            (r, buf)
        })
        .await
        .map_err(std::io::Error::other)
        .at(&self.ctx.part)?;
        written.at(&self.ctx.part)?;

        let n = buf.len() as u64;
        self.ctx.table.commit(self.id, n);
        self.offset += n;
        buf.clear();
        self.buf = buf;
        Ok(())
    }
}
