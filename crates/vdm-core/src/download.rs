//! Downloading one file: probe, pick a name, then either the multi-connection
//! path (server supports ranges and reports a size) or a single stream.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::HeaderMap;
use reqwest::{Client, StatusCode, Url};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::sync::CancellationToken;

use crate::auth::{ProxySetting, SiteLogin, apply_login};
use crate::error::{Error, IoContext, Result};
use crate::filename;
use crate::http;
use crate::limiter::{RateLimiter, acquire_all};
use crate::part::{Claims, PartMeta, Slot};
use crate::probe::{ProbeInfo, probe_once};
use crate::segment::SegmentTable;
use crate::segmented::{self, Ctx};

/// Most connections allowed for one download.
pub const MAX_CONNECTIONS: u8 = 32;

/// How many times one call restarts from zero because the file changed mid-download.
const MAX_RESTARTS: u32 = 2;

/// What to download and where.
#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub url: String,
    pub out_dir: PathBuf,
    /// Overrides the server's or URL's file name. Sanitized before use.
    pub file_name: Option<String>,
    /// Parallel connections; `None` uses [`Options::connections`].
    pub connections: Option<u8>,
    /// Extra request headers, e.g. `Referer` or `Cookie` from the browser.
    pub headers: Vec<(String, String)>,
    /// Speed limits applied to every byte, e.g. a global and a per-download one.
    pub limits: Vec<Arc<RateLimiter>>,
    /// Which variant to take from an HLS/DASH stream.
    pub quality: vdm_media::Quality,
}

impl DownloadRequest {
    pub fn new(url: impl Into<String>, out_dir: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            out_dir: out_dir.into(),
            file_name: None,
            connections: None,
            headers: Vec::new(),
            limits: Vec::new(),
            quality: vdm_media::Quality::default(),
        }
    }
}

pub use vdm_proto::SegmentProgress;

/// Reported about 10 times a second (multi-connection) or after every chunk (single stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// What the downloader settled on, reported once data transfer is about to begin
/// (again after a restart).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    /// Name the finished file will get (numbered if the plain name was taken).
    pub file_name: String,
    /// The in-progress file.
    pub part_path: PathBuf,
    pub size: Option<u64>,
    pub resumable: bool,
    /// Connections in use: 1 for servers that can't resume.
    pub connections: u8,
    /// Bytes already on disk from an earlier run.
    pub resumed_from: u64,
}

/// Receives updates while a download runs. Any `FnMut(Progress)` closure is an observer.
pub trait Observer: Send {
    fn started(&mut self, _info: &Started) {}
    /// The pieces of a multi-connection download and how many connections are
    /// transferring, sent just before each `progress`.
    fn segments(&mut self, _segments: Vec<SegmentProgress>, _connections: u8) {}
    fn progress(&mut self, progress: Progress);
}

impl<F: FnMut(Progress) + Send> Observer for F {
    fn progress(&mut self, progress: Progress) {
        self(progress)
    }
}

/// A finished download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed {
    pub path: PathBuf,
    pub size: u64,
    /// Bytes already on disk from an earlier run.
    pub resumed_from: u64,
    /// An earlier part was discarded because the file changed on the server,
    /// the server could not resume it, or its progress record was unusable.
    pub restarted: bool,
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Consecutive failed attempts without new data before a connection gives up.
    pub max_attempts: u32,
    pub base_delay: Duration,
    /// Cap on backoff and on server `Retry-After`.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Exponential backoff (1s, 2s, 4s, ...) unless the server asked for a delay.
    pub(crate) fn delay(&self, failures: u32, retry_after: Option<Duration>) -> Duration {
        let backoff = self.base_delay.saturating_mul(1 << failures.saturating_sub(1).min(16));
        retry_after.unwrap_or(backoff).min(self.max_delay)
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub user_agent: String,
    pub connect_timeout: Duration,
    /// Max time without receiving any data.
    pub read_timeout: Duration,
    pub retry: RetryPolicy,
    /// If the remote file changed, start over (true) or fail with
    /// [`Error::RemoteChanged`] (false).
    pub restart_if_changed: bool,
    /// Bytes buffered in memory per connection before writing to disk.
    pub write_buffer: usize,
    /// Default parallel connections per download (1..=[`MAX_CONNECTIONS`]).
    pub connections: u8,
    /// Segments are never split below this size.
    pub min_segment: u64,
    /// Media segments fetched at the same time (HLS/DASH).
    pub media_concurrency: usize,
    /// ffmpeg for joining video and audio into one .mp4; `None` looks on PATH.
    pub ffmpeg: Option<PathBuf>,
    /// Use ffmpeg (when found) to produce one .mp4 from HLS/DASH streams.
    pub remux: bool,
    pub proxy: ProxySetting,
    /// Credentials sent to matching sites (HTTP Basic auth, FTP login).
    pub logins: Vec<SiteLogin>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            user_agent: concat!("vdm/", env!("CARGO_PKG_VERSION")).to_owned(),
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            restart_if_changed: true,
            write_buffer: 256 * 1024,
            connections: 8,
            min_segment: 1024 * 1024,
            media_concurrency: 6,
            ffmpeg: None,
            remux: true,
            proxy: ProxySetting::System,
            logins: Vec::new(),
        }
    }
}

/// Downloads files over HTTP(S), using several connections when the server allows it.
#[derive(Debug, Clone)]
pub struct Downloader {
    client: Client,
    opts: Options,
    /// Part files in use, so concurrent downloads of the same name never share one.
    claims: Claims,
}

impl Downloader {
    pub fn new(opts: Options) -> Result<Self> {
        let mut builder = Client::builder()
            .user_agent(&opts.user_agent)
            .connect_timeout(opts.connect_timeout)
            .read_timeout(opts.read_timeout);
        builder = match &opts.proxy {
            ProxySetting::System => builder,
            ProxySetting::None => builder.no_proxy(),
            ProxySetting::Url(url) => {
                builder.proxy(reqwest::Proxy::all(url).map_err(Error::Network)?)
            }
        };
        let client = builder.build().map_err(Error::Network)?;
        Ok(Self { client, opts, claims: Claims::default() })
    }

    /// Ask the server about `url` (size, range support, validators, name), with retries.
    pub async fn probe(
        &self,
        url: &str,
        headers: &[(String, String)],
        cancel: &CancellationToken,
    ) -> Result<ProbeInfo> {
        let url = parse_url(url)?;
        if crate::ftp_download::is_ftp(&url) {
            return self.probe_ftp_info(&url, cancel).await;
        }
        let mut headers = http::build_headers(headers)?;
        apply_login(&mut headers, &url, &self.opts.logins);
        self.probe_parsed(&url, &headers, cancel).await
    }

    async fn probe_parsed(
        &self,
        url: &Url,
        headers: &HeaderMap,
        cancel: &CancellationToken,
    ) -> Result<ProbeInfo> {
        let mut failures = 0;
        loop {
            let result = tokio::select! {
                r = probe_once(&self.client, url, headers) => r,
                () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: 0 }),
            };
            match result {
                Ok(info) => return Ok(info),
                Err(err) => {
                    failures += 1;
                    self.backoff(err, failures, cancel, 0).await?;
                }
            }
        }
    }

    /// Download `req`, resuming an earlier `.vdmpart` for the same URL if the
    /// file is unchanged. On cancel the part and its progress are kept and
    /// [`Error::Cancelled`] is returned; calling again resumes it.
    pub async fn download<O>(
        &self,
        req: DownloadRequest,
        mut observer: O,
        cancel: CancellationToken,
    ) -> Result<Completed>
    where
        O: Observer,
    {
        let url = parse_url(&req.url)?;
        if crate::ftp_download::is_ftp(&url) {
            return self.download_ftp(&req, &url, &mut observer, &cancel).await;
        }
        let mut headers = http::build_headers(&req.headers)?;
        apply_login(&mut headers, &url, &self.opts.logins);
        if vdm_media::is_media_url(url.as_str()) {
            return self.download_media(&req, &url, &headers, &mut observer, &cancel).await;
        }
        let mut restarts = 0;
        loop {
            let fresh = restarts > 0;
            match self.download_once(&req, &url, &headers, fresh, &mut observer, &cancel).await {
                Err(Error::RemoteChanged)
                    if self.opts.restart_if_changed && restarts < MAX_RESTARTS =>
                {
                    tracing::warn!(url = %url, "file changed on the server; starting over");
                    restarts += 1;
                }
                Ok(mut done) => {
                    done.restarted |= fresh;
                    return Ok(done);
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// One pass: probe, choose the slot, download. `fresh` discards any earlier part.
    async fn download_once<O>(
        &self,
        req: &DownloadRequest,
        url: &Url,
        headers: &HeaderMap,
        fresh: bool,
        observer: &mut O,
        cancel: &CancellationToken,
    ) -> Result<Completed>
    where
        O: Observer,
    {
        let info = self.probe_parsed(url, headers, cancel).await?;
        if info.content_type.as_deref().is_some_and(crate::media::is_playlist_type) {
            return self.download_media(req, url, headers, observer, cancel).await;
        }
        let base = match &req.file_name {
            Some(name) => filename::sanitize(name),
            None => info.file_name.clone(),
        };
        tokio::fs::create_dir_all(&req.out_dir).await.at(&req.out_dir)?;
        // Held until this function returns, i.e. until the part is renamed or kept.
        let (slot, _claim) = Slot::claim(&req.out_dir, &base, url.as_str(), &self.claims).await?;
        let current =
            PartMeta::new(url.as_str(), info.size, info.etag.clone(), info.last_modified.clone());

        match (info.resumable, info.size) {
            (true, Some(size)) => {
                let connections =
                    req.connections.unwrap_or(self.opts.connections).clamp(1, MAX_CONNECTIONS);
                let job = Job { url, headers, limits: &req.limits, slot: &slot, current, fresh };
                self.download_segmented(job, size, connections, observer, cancel).await
            }
            _ => {
                if slot.previous.is_some() {
                    tracing::warn!(url = %url, "server can't resume this file; starting over");
                }
                let restarted = slot.previous.is_some();
                let stream = Stream { url, headers, limits: &req.limits, slot: &slot };
                let mut done = self.download_stream(stream, current, observer, cancel).await?;
                done.restarted |= restarted;
                Ok(done)
            }
        }
    }

    async fn download_segmented<O>(
        &self,
        job: Job<'_>,
        size: u64,
        connections: u8,
        observer: &mut O,
        cancel: &CancellationToken,
    ) -> Result<Completed>
    where
        O: Observer,
    {
        let Job { url, headers, limits, slot, current, fresh } = job;
        let min_segment = self.opts.min_segment.max(1);

        // Reuse the earlier part only if it is the same file and its record is sound.
        let mut restarted = false;
        let mut table = None;
        if let Some(prev) = &slot.previous {
            restarted = true;
            if fresh {
                // The caller saw the file change mid-download.
            } else if prev.differs(&current) {
                if !self.opts.restart_if_changed {
                    return Err(Error::RemoteChanged);
                }
                tracing::warn!(url = %url, "file changed on the server; starting over");
            } else if tokio::fs::metadata(&slot.part).await.ok().map(|m| m.len()) != Some(size) {
                tracing::warn!(path = %slot.part.display(), "part has the wrong size; starting over");
            } else if let Some(t) = SegmentTable::from_snapshot(size, min_segment, &prev.segments) {
                table = Some(t);
                restarted = false;
            } else {
                tracing::warn!(path = %slot.part.display(), "unusable progress record; starting over");
            }
        }
        let resume = table.is_some();
        let table =
            table.unwrap_or_else(|| SegmentTable::new(size, usize::from(connections), min_segment));
        let resumed_from = if resume { table.downloaded() } else { 0 };

        let mut meta = current;
        meta.segments = table.snapshot();
        if !resume {
            // Sidecar first: a part without one is never adopted later.
            slot.write_meta(&meta).await?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(!resume)
            .open(&slot.part)
            .await
            .at(&slot.part)?;
        // Reserve the full size so every connection can write at its own offset.
        file.set_len(size).await.at(&slot.part)?;
        let file = file.into_std().await;

        if resume {
            tracing::info!(url = %url, resumed_from, "resuming");
        }
        tracing::debug!(url = %url, size, connections, "starting segmented download");
        observer.started(&Started {
            file_name: slot.name(),
            part_path: slot.part.clone(),
            size: Some(size),
            resumable: true,
            connections,
            resumed_from,
        });
        let ctx = Arc::new(Ctx {
            client: self.client.clone(),
            url: url.clone(),
            headers: headers.clone(),
            remote: meta.clone(),
            table,
            file,
            part: slot.part.clone(),
            retry: self.opts.retry.clone(),
            write_buffer: self.opts.write_buffer.max(1),
            limits: limits.to_vec(),
        });
        segmented::run(ctx.clone(), usize::from(connections), slot, &mut meta, observer, cancel)
            .await?;

        let sync_ctx = ctx.clone();
        tokio::task::spawn_blocking(move || sync_ctx.file.sync_all())
            .await
            .map_err(std::io::Error::other)
            .and_then(|r| r)
            .at(&slot.part)?;
        // Close the file before renaming it.
        drop(ctx);
        let path = slot.finish().await?;
        tracing::info!(path = %path.display(), size, connections, "download complete");
        Ok(Completed { path, size, resumed_from, restarted })
    }

    /// Single stream from the start, for servers that can't resume or don't
    /// report a size. Retries start over from zero.
    async fn download_stream<O>(
        &self,
        s: Stream<'_>,
        meta: PartMeta,
        observer: &mut O,
        cancel: &CancellationToken,
    ) -> Result<Completed>
    where
        O: Observer,
    {
        let part = &s.slot.part;
        s.slot.write_meta(&meta).await?;
        let file =
            OpenOptions::new().write(true).create(true).truncate(true).open(part).await.at(part)?;
        let mut writer = BufWriter::with_capacity(self.opts.write_buffer.max(1), file);
        observer.started(&Started {
            file_name: s.slot.name(),
            part_path: part.clone(),
            size: meta.size,
            resumable: false,
            connections: 1,
            resumed_from: 0,
        });

        let mut best = 0;
        let mut failures = 0;
        loop {
            let mut written = 0;
            match self.stream_once(&s, &mut writer, &mut written, observer, cancel).await {
                Ok(()) => break,
                Err(err) => {
                    writer.flush().await.at(part)?;
                    if matches!(err, Error::Cancelled { .. }) {
                        return Err(Error::Cancelled { downloaded: written });
                    }
                    if written > best {
                        best = written;
                        failures = 0;
                    }
                    failures += 1;
                    self.backoff(err, failures, cancel, written).await?;
                }
            }
        }

        writer.flush().await.at(part)?;
        let file = writer.into_inner();
        let size = file.metadata().await.at(part)?.len();
        file.sync_all().await.at(part)?;
        drop(file);
        let path = s.slot.finish().await?;
        tracing::info!(path = %path.display(), size, "download complete");
        Ok(Completed { path, size, resumed_from: 0, restarted: false })
    }

    /// One full `GET`, written from the start of the part.
    async fn stream_once<O>(
        &self,
        s: &Stream<'_>,
        writer: &mut BufWriter<tokio::fs::File>,
        written: &mut u64,
        observer: &mut O,
        cancel: &CancellationToken,
    ) -> Result<()>
    where
        O: Observer,
    {
        use tokio::io::AsyncSeekExt;
        let part = &s.slot.part;
        writer.flush().await.at(part)?;
        writer.get_mut().set_len(0).await.at(part)?;
        writer.seek(std::io::SeekFrom::Start(0)).await.at(part)?;

        let req = self.client.get(s.url.clone()).headers(s.headers.clone());
        let mut resp = tokio::select! {
            r = req.send() => r.map_err(Error::Network)?,
            () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: 0 }),
        };
        let status = resp.status();
        if status != StatusCode::OK {
            return Err(Error::Status { status, retry_after: http::retry_after(resp.headers()) });
        }
        let total = http::content_length(resp.headers());
        observer.progress(Progress { downloaded: 0, total });
        loop {
            let chunk = tokio::select! {
                c = resp.chunk() => c.map_err(Error::Network)?,
                () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: *written }),
            };
            let Some(chunk) = chunk else { break };
            let len = chunk.len() as u64;
            if total.is_some_and(|t| *written + len > t) {
                return Err(Error::Protocol("server sent more data than the file size".into()));
            }
            if !s.limits.is_empty() {
                tokio::select! {
                    () = acquire_all(s.limits, len) => {}
                    () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: *written }),
                }
            }
            writer.write_all(&chunk).await.at(part)?;
            *written += len;
            observer.progress(Progress { downloaded: *written, total });
        }
        match total {
            Some(t) if *written < t => Err(Error::Incomplete { received: *written, expected: t }),
            _ => Ok(()),
        }
    }

    /// Sleep before the next attempt, or return `err` if it is fatal or we are out of attempts.
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn options(&self) -> &Options {
        &self.opts
    }

    pub(crate) fn claims(&self) -> &Claims {
        &self.claims
    }

    pub(crate) async fn backoff(
        &self,
        err: Error,
        failures: u32,
        cancel: &CancellationToken,
        downloaded: u64,
    ) -> Result<()> {
        if !err.is_transient() || failures >= self.opts.retry.max_attempts {
            return Err(err);
        }
        let delay = self.opts.retry.delay(failures, err.retry_after());
        tracing::warn!(%err, failures, ?delay, "retrying");
        tokio::select! {
            () = tokio::time::sleep(delay) => Ok(()),
            () = cancel.cancelled() => Err(Error::Cancelled { downloaded }),
        }
    }
}

/// Inputs of the segmented path, bundled to keep signatures short.
struct Job<'a> {
    url: &'a Url,
    headers: &'a HeaderMap,
    limits: &'a [Arc<RateLimiter>],
    slot: &'a Slot,
    current: PartMeta,
    fresh: bool,
}

/// Inputs of the single-stream path.
struct Stream<'a> {
    url: &'a Url,
    headers: &'a HeaderMap,
    limits: &'a [Arc<RateLimiter>],
    slot: &'a Slot,
}

pub(crate) fn parse_url(url: &str) -> Result<Url> {
    let parsed = Url::parse(url.trim()).map_err(|_| Error::InvalidUrl(url.to_owned()))?;
    match parsed.scheme() {
        "http" | "https" | "ftp" | "ftps" => Ok(parsed),
        _ => Err(Error::InvalidUrl(url.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule() {
        let p = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
        };
        let secs: Vec<u64> = (1..=5).map(|n| p.delay(n, None).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 8]);
        assert_eq!(p.delay(1, Some(Duration::from_secs(3))), Duration::from_secs(3));
        assert_eq!(p.delay(1, Some(Duration::from_secs(60))), Duration::from_secs(8));
    }

    #[test]
    fn url_validation() {
        assert!(parse_url("https://example.com/a").is_ok());
        assert!(parse_url(" http://example.com/a ").is_ok());
        assert!(parse_url("ftp://example.com/a").is_ok());
        assert!(matches!(parse_url("file:///etc/passwd"), Err(Error::InvalidUrl(_))));
        assert!(matches!(parse_url("not a url"), Err(Error::InvalidUrl(_))));
    }
}
