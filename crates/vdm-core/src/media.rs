//! Downloading HLS and DASH streams.
//!
//! Fetch the playlist or manifest, pick a variant, fetch every segment
//! (several at a time, with retries and speed limits), decrypt AES-128, and
//! keep each finished segment as its own file in a `<name>.vdmpart` folder so
//! an interrupted download resumes where it stopped. When all segments are
//! there, join them into one file per track; with ffmpeg available, mux the
//! tracks (or convert a `.ts`) into a single `.mp4`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use reqwest::header::{self, HeaderMap};
use reqwest::{StatusCode, Url};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use vdm_media::{
    self as media, Container, Encryption, Quality, Track, TrackKind, Variant, dash, hls,
};

use crate::download::{Completed, DownloadRequest, Downloader, Observer, Progress, Started};
use crate::error::{Error, IoContext, Result};
use crate::filename;
use crate::http;
use crate::limiter::{RateLimiter, acquire_all};
use crate::part::{PartMeta, Slot};

/// How often progress is reported.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Largest playlist or manifest accepted.
const MAX_MANIFEST: usize = 16 * 1024 * 1024;

/// Content types of HLS playlists and DASH manifests.
pub fn is_playlist_type(content_type: &str) -> bool {
    let mime = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-mpegurl"
            | "application/dash+xml"
    )
}

/// The streaming format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Hls,
    Dash,
}

/// What a stream offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfo {
    pub kind: MediaKind,
    /// Selectable qualities (video, or audio for audio-only streams).
    pub variants: Vec<Variant>,
    /// Index into `variants` that [`Quality`] picked.
    pub chosen: Option<usize>,
    /// A separate audio track is downloaded along with the video.
    pub separate_audio: bool,
    /// Playback length in ms (0 when unknown).
    pub duration_ms: u64,
}

/// A resolved stream: what to fetch, and how it was described.
struct Plan {
    info: MediaInfo,
    tracks: Vec<Track>,
}

/// One file to fetch into the work folder.
#[derive(Clone)]
struct Job {
    segment: media::Segment,
    file: PathBuf,
}

impl Downloader {
    /// Describe an HLS/DASH stream: its qualities and which one `quality` picks.
    pub async fn probe_media(
        &self,
        url: &str,
        headers: &[(String, String)],
        quality: Quality,
        cancel: &CancellationToken,
    ) -> Result<MediaInfo> {
        let url = crate::download::parse_url(url)?;
        let mut headers = http::build_headers(headers)?;
        crate::auth::apply_login(&mut headers, &url, &self.options().logins);
        Ok(self.plan(&url, &headers, quality, cancel).await?.info)
    }

    pub(crate) async fn download_media<O: Observer>(
        &self,
        req: &DownloadRequest,
        url: &Url,
        headers: &HeaderMap,
        observer: &mut O,
        cancel: &CancellationToken,
    ) -> Result<Completed> {
        let plan = self.plan(url, headers, req.quality, cancel).await?;
        let ffmpeg = self.ffmpeg();
        let remux = ffmpeg.is_some() && needs_remux(&plan.tracks);
        let main = &plan.tracks[0];
        let ext = if remux {
            if main.kind == TrackKind::Audio { "m4a" } else { "mp4" }
        } else {
            main.container.extension(main.kind)
        };
        let stem = match &req.file_name {
            Some(name) => strip_media_extension(&filename::sanitize(name)),
            None => stream_name(url),
        };
        let final_name = format!("{stem}.{ext}");

        tokio::fs::create_dir_all(&req.out_dir).await.at(&req.out_dir)?;
        let (slot, _claim) =
            Slot::claim(&req.out_dir, &final_name, url.as_str(), self.claims()).await?;
        let work = slot.part.clone();
        let fingerprint = fingerprint(&plan.tracks);
        let resume = slot.previous.is_some() && same_plan(&work, &fingerprint).await;
        if !resume {
            if tokio::fs::try_exists(&work).await.at(&work)? {
                tokio::fs::remove_dir_all(&work).await.at(&work)?;
            }
            slot.write_meta(&PartMeta::new(url.as_str(), None, None, None)).await?;
            tokio::fs::create_dir_all(&work).await.at(&work)?;
            tokio::fs::write(work.join("plan.txt"), &fingerprint).await.at(&work)?;
        }

        // Every init and media segment of every track, as files in the work folder.
        let mut jobs = Vec::new();
        for (t, track) in plan.tracks.iter().enumerate() {
            if let Some(init) = &track.init {
                jobs.push(Job { segment: init.clone(), file: work.join(format!("{t}-init.seg")) });
            }
            for (i, seg) in track.segments.iter().enumerate() {
                jobs.push(Job { segment: seg.clone(), file: work.join(format!("{t}-{i:06}.seg")) });
            }
        }
        let total_jobs = jobs.len() as u64;
        let mut todo = Vec::new();
        let mut have_bytes = 0;
        let mut have = 0u64;
        for job in jobs {
            match tokio::fs::metadata(&job.file).await {
                Ok(meta) => {
                    have += 1;
                    have_bytes += meta.len();
                }
                Err(_) => todo.push(job),
            }
        }
        let concurrency = self.options().media_concurrency.max(1);
        observer.started(&Started {
            file_name: slot.name(),
            part_path: work.clone(),
            size: None,
            resumable: true,
            connections: u8::try_from(concurrency).unwrap_or(u8::MAX),
            resumed_from: have_bytes,
        });
        tracing::info!(
            url = %url, segments = total_jobs, done = have, tracks = plan.tracks.len(),
            "downloading stream"
        );

        let keys = self.fetch_keys(&plan.tracks, url, headers, &req.limits, cancel).await?;
        self.fetch_segments(
            todo,
            keys,
            (url, headers),
            &req.limits,
            (have, have_bytes, total_jobs),
            observer,
            cancel,
        )
        .await?;

        // Join each track, then mux with ffmpeg when possible.
        let mut outputs = Vec::new();
        for (t, track) in plan.tracks.iter().enumerate() {
            let out = work.join(format!("track{t}.{}", track.container.extension(track.kind)));
            join_track(&work, t, track, &out).await?;
            outputs.push((out, track.clone()));
        }
        let mut finished = None;
        if remux && let Some(ffmpeg) = &ffmpeg {
            let out = work.join(format!("out.{ext}"));
            match run_ffmpeg(ffmpeg, &outputs, &out).await {
                Ok(()) => finished = Some(out),
                Err(err) => tracing::warn!(%err, "ffmpeg failed; keeping the separate tracks"),
            }
        }
        let path = match finished {
            Some(out) => slot.finish_from(&out).await?,
            None => {
                // No muxing: the first track takes the name; others go beside it.
                let main = slot.finish_from(&outputs[0].0).await?;
                for (out, track) in &outputs[1..] {
                    let stem = main.file_stem().unwrap_or_default().to_string_lossy();
                    let name = format!("{stem}.{}", track.container.extension(track.kind));
                    let target = free_path(&req.out_dir, &name).await?;
                    tokio::fs::rename(out, &target).await.at(&target)?;
                }
                main
            }
        };
        if let Err(err) = tokio::fs::remove_dir_all(&work).await {
            tracing::warn!(path = %work.display(), %err, "could not delete the work folder");
        }
        let size = tokio::fs::metadata(&path).await.at(&path)?.len();
        observer.progress(Progress { downloaded: size, total: Some(size) });
        tracing::info!(path = %path.display(), size, "stream saved");
        Ok(Completed { path, size, resumed_from: have_bytes, restarted: false, seed: None })
    }

    /// Fetch the playlist/manifest (and, for HLS masters, the chosen variant
    /// and audio playlists) and turn it into tracks.
    async fn plan(
        &self,
        url: &Url,
        headers: &HeaderMap,
        quality: Quality,
        cancel: &CancellationToken,
    ) -> Result<Plan> {
        let text = self.fetch_text(url, headers, cancel).await?;
        let bad = |e: media::ParseError| Error::Protocol(e.to_string());
        let mut info = MediaInfo {
            kind: MediaKind::Hls,
            variants: Vec::new(),
            chosen: None,
            separate_audio: false,
            duration_ms: 0,
        };
        let mut tracks = Vec::new();
        if hls::is_playlist(&text) {
            if hls::is_master(&text) {
                let master = hls::parse_master(&text, url).map_err(bad)?;
                info.variants = master.variants.iter().map(|v| v.variant.clone()).collect();
                let i = media::choose(&info.variants, quality)
                    .ok_or_else(|| Error::Protocol("the playlist has no variants".into()))?;
                info.chosen = Some(i);
                let chosen = &master.variants[i];
                let audio = hls::audio_for(&master, chosen).and_then(|r| r.uri.clone());
                let kind = if audio.is_some() { TrackKind::Video } else { TrackKind::Muxed };
                let scoped = self.scoped(headers, url, &chosen.uri);
                tracks.push(self.media_playlist(&chosen.uri, kind, &scoped, cancel).await?);
                if let Some(audio) = audio {
                    info.separate_audio = true;
                    tracks.push(
                        self.media_playlist(
                            &audio,
                            TrackKind::Audio,
                            &self.scoped(headers, url, &audio),
                            cancel,
                        )
                        .await?,
                    );
                }
            } else {
                let playlist = hls::parse_media(&text, url, TrackKind::Muxed).map_err(bad)?;
                if !playlist.ended {
                    return Err(live());
                }
                tracks.push(playlist.track);
            }
        } else if dash::is_mpd(&text) {
            info.kind = MediaKind::Dash;
            let manifest = dash::parse(&text, url).map_err(bad)?;
            let audio = dash::best_audio(&manifest).map(|r| r.track.clone());
            let reps = if manifest.video.is_empty() { &manifest.audio } else { &manifest.video };
            info.variants = reps.iter().map(|r| r.variant.clone()).collect();
            let i = media::choose(&info.variants, quality)
                .ok_or_else(|| Error::Protocol("the manifest has no video or audio".into()))?;
            info.chosen = Some(i);
            tracks.push(reps[i].track.clone());
            if !manifest.video.is_empty()
                && let Some(audio) = audio
            {
                info.separate_audio = true;
                tracks.push(audio);
            }
        } else {
            return Err(Error::Protocol("not an HLS playlist or DASH manifest".into()));
        }
        if tracks.iter().all(|t| t.segments.is_empty()) {
            return Err(Error::Protocol("the stream has no segments".into()));
        }
        info.duration_ms = tracks[0].duration_ms();
        Ok(Plan { info, tracks })
    }

    async fn media_playlist(
        &self,
        url: &Url,
        kind: TrackKind,
        headers: &HeaderMap,
        cancel: &CancellationToken,
    ) -> Result<Track> {
        let text = self.fetch_text(url, headers, cancel).await?;
        let playlist =
            hls::parse_media(&text, url, kind).map_err(|e| Error::Protocol(e.to_string()))?;
        if !playlist.ended {
            return Err(live());
        }
        Ok(playlist.track)
    }

    async fn fetch_text(
        &self,
        url: &Url,
        headers: &HeaderMap,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let bytes = self.fetch(url, None, headers, &[], cancel).await?;
        if bytes.len() > MAX_MANIFEST {
            return Err(Error::Protocol(format!("{url} is too large for a playlist")));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Download every distinct AES key used by the tracks.
    async fn fetch_keys(
        &self,
        tracks: &[Track],
        root: &Url,
        headers: &HeaderMap,
        limits: &[Arc<RateLimiter>],
        cancel: &CancellationToken,
    ) -> Result<Arc<HashMap<Url, [u8; 16]>>> {
        let mut urls = HashSet::new();
        for track in tracks {
            for seg in track.init.iter().chain(&track.segments) {
                if let Encryption::Aes128 { key_url, .. } = &seg.encryption {
                    urls.insert(key_url.clone());
                }
            }
        }
        let mut keys = HashMap::new();
        for url in urls {
            let scoped = self.scoped(headers, root, &url);
            let bytes = self.fetch(&url, None, &scoped, limits, cancel).await?;
            let key: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
                Error::Protocol(format!("the key at {url} is {} bytes, not 16", bytes.len()))
            })?;
            keys.insert(url, key);
        }
        Ok(Arc::new(keys))
    }

    /// Fetch `jobs` with `media_concurrency` requests at a time, reporting
    /// progress as done segments plus an estimated total.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_segments<O: Observer>(
        &self,
        jobs: Vec<Job>,
        keys: Arc<HashMap<Url, [u8; 16]>>,
        (root, headers): (&Url, &HeaderMap),
        limits: &[Arc<RateLimiter>],
        (done_before, bytes_before, total_jobs): (u64, u64, u64),
        observer: &mut O,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let stop = cancel.child_token();
        let bytes = Arc::new(AtomicU64::new(bytes_before));
        let done = Arc::new(AtomicU64::new(done_before));
        let permits = Arc::new(Semaphore::new(self.options().media_concurrency.max(1)));
        let mut set = JoinSet::new();
        for job in jobs {
            let headers = self.scoped(headers, root, &job.segment.url);
            let (dl, keys, limits) = (self.clone(), keys.clone(), limits.to_vec());
            let (bytes, done, permits, stop) =
                (bytes.clone(), done.clone(), permits.clone(), stop.clone());
            set.spawn(async move {
                let _permit = permits
                    .acquire_owned()
                    .await
                    .map_err(|_| Error::Cancelled { downloaded: 0 })?;
                if stop.is_cancelled() {
                    return Err(Error::Cancelled { downloaded: 0 });
                }
                let n = dl.fetch_segment(&job, &keys, &headers, &limits, &stop).await?;
                bytes.fetch_add(n, Ordering::Relaxed);
                done.fetch_add(1, Ordering::Relaxed);
                Ok::<(), Error>(())
            });
        }

        let report = |observer: &mut O| {
            let (b, d) = (bytes.load(Ordering::Relaxed), done.load(Ordering::Relaxed));
            // Estimate the total from the average segment size so far.
            let total = (d > 0).then(|| b / d * total_jobs).map(|t| t.max(b));
            observer.progress(Progress { downloaded: b, total });
        };
        let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut fatal = None;
        loop {
            tokio::select! {
                joined = set.join_next() => {
                    let Some(joined) = joined else { break };
                    let err = match joined {
                        Ok(Ok(()) | Err(Error::Cancelled { .. })) => continue,
                        Ok(Err(err)) => err,
                        Err(join) => Error::Io { path: PathBuf::new(), source: std::io::Error::other(join) },
                    };
                    if fatal.is_none() {
                        stop.cancel();
                        fatal = Some(err);
                    }
                }
                _ = tick.tick() => report(observer),
            }
        }
        report(observer);
        if let Some(err) = fatal {
            return Err(err);
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled { downloaded: bytes.load(Ordering::Relaxed) });
        }
        Ok(())
    }

    /// Fetch, decrypt and store one segment (temp file, then rename, so a
    /// half-written segment never counts as done). Returns its stored size.
    async fn fetch_segment(
        &self,
        job: &Job,
        keys: &HashMap<Url, [u8; 16]>,
        headers: &HeaderMap,
        limits: &[Arc<RateLimiter>],
        cancel: &CancellationToken,
    ) -> Result<u64> {
        let seg = &job.segment;
        let mut data = self.fetch(&seg.url, seg.range, headers, limits, cancel).await?;
        if let Encryption::Aes128 { key_url, iv } = &seg.encryption {
            let key = keys
                .get(key_url)
                .ok_or_else(|| Error::Protocol(format!("no key for {}", seg.url)))?;
            decrypt(&mut data, key, iv).map_err(|()| {
                Error::Protocol(format!("could not decrypt {} (wrong key?)", seg.url))
            })?;
        }
        let tmp = job.file.with_extension("tmp");
        tokio::fs::write(&tmp, &data).await.at(&tmp)?;
        tokio::fs::rename(&tmp, &job.file).await.at(&job.file)?;
        Ok(data.len() as u64)
    }

    /// GET a whole resource or a byte range into memory, with retries.
    async fn fetch(
        &self,
        url: &Url,
        range: Option<(u64, u64)>,
        headers: &HeaderMap,
        limits: &[Arc<RateLimiter>],
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        let mut failures = 0;
        loop {
            let attempt = tokio::select! {
                r = self.fetch_once(url, range, headers, limits, cancel) => r,
                () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: 0 }),
            };
            match attempt {
                Ok(data) => return Ok(data),
                Err(err) => {
                    failures += 1;
                    self.backoff(err, failures, cancel, 0).await?;
                }
            }
        }
    }

    async fn fetch_once(
        &self,
        url: &Url,
        range: Option<(u64, u64)>,
        headers: &HeaderMap,
        limits: &[Arc<RateLimiter>],
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        let mut req = self.client().get(url.clone()).headers(headers.clone());
        if let Some((offset, len)) = range {
            let last = offset + len.saturating_sub(1);
            req = req.header(header::RANGE, format!("bytes={offset}-{last}"));
        }
        let mut resp = req.send().await.map_err(Error::Network)?;
        let status = resp.status();
        let whole_body = match (status, range) {
            (StatusCode::PARTIAL_CONTENT, Some(_)) => false,
            (StatusCode::OK, _) => true,
            _ => {
                return Err(Error::Status {
                    status,
                    retry_after: http::retry_after(resp.headers()),
                });
            }
        };
        let mut data =
            Vec::with_capacity(http::content_length(resp.headers()).unwrap_or(0) as usize);
        loop {
            let chunk = tokio::select! {
                c = resp.chunk() => c.map_err(Error::Network)?,
                () = cancel.cancelled() => return Err(Error::Cancelled { downloaded: 0 }),
            };
            let Some(chunk) = chunk else { break };
            if !limits.is_empty() {
                acquire_all(limits, chunk.len() as u64).await;
            }
            data.extend_from_slice(&chunk);
        }
        if let (true, Some((offset, len))) = (whole_body, range) {
            // The server ignored the range: cut it out of the whole resource.
            let (start, end) = (offset as usize, (offset + len) as usize);
            if end > data.len() {
                return Err(Error::Protocol(format!("{url} is shorter than the byte range")));
            }
            data = data[start..end].to_vec();
        }
        Ok(data)
    }

    /// Headers for a request to `target` within the stream at `root`: the
    /// caller's cookies and credentials only go to the stream's own host
    /// (segments often come from a CDN), plus a saved login for `target`'s host.
    fn scoped(&self, headers: &HeaderMap, root: &Url, target: &Url) -> HeaderMap {
        let mut scoped = headers.clone();
        if root.host_str() != target.host_str() {
            scoped.remove(header::AUTHORIZATION);
            scoped.remove(header::COOKIE);
        }
        crate::auth::apply_login(&mut scoped, target, &self.options().logins);
        scoped
    }

    /// ffmpeg from the options, else from PATH.
    fn ffmpeg(&self) -> Option<PathBuf> {
        if !self.options().remux {
            return None;
        }
        if let Some(path) = &self.options().ffmpeg {
            return Some(path.clone());
        }
        let exe = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(&exe))
            .find(|p| p.is_file())
    }
}

fn live() -> Error {
    Error::Protocol("live streams are not supported yet".into())
}

/// AES-128-CBC with PKCS#7 padding, in place.
fn decrypt(data: &mut Vec<u8>, key: &[u8; 16], iv: &[u8; 16]) -> Result<(), ()> {
    let len = cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(data)
        .map_err(|_| ())?
        .len();
    data.truncate(len);
    Ok(())
}

/// Two tracks always need muxing; a single `.ts` is converted to `.mp4`.
fn needs_remux(tracks: &[Track]) -> bool {
    tracks.len() > 1 || tracks.iter().any(|t| t.container == Container::Ts)
}

/// Identifies the segment list, so a resume never mixes two different plans.
fn fingerprint(tracks: &[Track]) -> String {
    let mut out = String::new();
    for (t, track) in tracks.iter().enumerate() {
        for seg in track.init.iter().chain(&track.segments) {
            out.push_str(&format!("{t} {} {:?}\n", seg.url, seg.range));
        }
    }
    out
}

async fn same_plan(work: &Path, fingerprint: &str) -> bool {
    tokio::fs::read_to_string(work.join("plan.txt")).await.is_ok_and(|saved| saved == fingerprint)
}

/// Append the init segment and all media segments of track `t` into `out`.
async fn join_track(work: &Path, t: usize, track: &Track, out: &Path) -> Result<()> {
    let mut file = tokio::fs::File::create(out).await.at(out)?;
    let mut parts = Vec::new();
    if track.init.is_some() {
        parts.push(work.join(format!("{t}-init.seg")));
    }
    parts.extend((0..track.segments.len()).map(|i| work.join(format!("{t}-{i:06}.seg"))));
    for part in parts {
        let mut src = tokio::fs::File::open(&part).await.at(&part)?;
        tokio::io::copy(&mut src, &mut file).await.at(out)?;
    }
    file.flush().await.at(out)?;
    file.sync_all().await.at(out)?;
    Ok(())
}

/// Mux the joined tracks into `out` without re-encoding.
async fn run_ffmpeg(ffmpeg: &Path, inputs: &[(PathBuf, Track)], out: &Path) -> anyhow_like::Result {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    for (path, _) in inputs {
        cmd.arg("-i").arg(path);
    }
    for i in 0..inputs.len() {
        cmd.args(["-map", &i.to_string()]);
    }
    cmd.args(["-c", "copy"]);
    // ADTS AAC (from .ts / .aac) must be repackaged to go into MP4.
    if inputs.iter().any(|(_, t)| matches!(t.container, Container::Ts | Container::Raw("aac"))) {
        cmd.args(["-bsf:a", "aac_adtstoasc"]);
    }
    cmd.args(["-movflags", "+faststart"]).arg(out);
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let output = cmd.output().await.map_err(|e| format!("cannot run {}: {e}", ffmpeg.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// Small result alias for the ffmpeg step, whose errors are only logged.
mod anyhow_like {
    pub type Result = std::result::Result<(), String>;
}

/// A name for the saved stream from its URL: the playlist's file name, or
/// its folder when the file name is generic (`master.m3u8`, `index.m3u8`, ...).
fn stream_name(url: &Url) -> String {
    const GENERIC: &[&str] = &[
        "master",
        "index",
        "playlist",
        "manifest",
        "prog_index",
        "main",
        "chunklist",
        "stream",
        "video",
        "media",
        "hls",
        "dash",
    ];
    let segments: Vec<&str> =
        url.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    let mut stems = segments.iter().rev().map(|seg| {
        let decoded = percent_decode(seg);
        strip_media_extension(&decoded)
    });
    let first = stems.next().unwrap_or_default();
    let name = if GENERIC.contains(&first.to_ascii_lowercase().as_str()) || first.is_empty() {
        stems
            .find(|s| !s.is_empty())
            .unwrap_or_else(|| url.host_str().unwrap_or("video").to_owned())
    } else {
        first
    };
    let clean = filename::sanitize(&name);
    if clean == filename::FALLBACK_NAME { "video".to_owned() } else { clean }
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// `name.m3u8` → `name`; other extensions are part of the name.
fn strip_media_extension(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for ext in [".m3u8", ".m3u", ".mpd", ".mp4", ".m4a", ".ts", ".webm", ".mkv"] {
        if lower.ends_with(ext) && name.len() > ext.len() {
            return name[..name.len() - ext.len()].to_owned();
        }
    }
    name.to_owned()
}

/// `dir/name`, or `dir/name (n)` if taken.
async fn free_path(dir: &Path, name: &str) -> Result<PathBuf> {
    for n in 0..10_000 {
        let path = dir.join(filename::numbered(name, n));
        if !tokio::fs::try_exists(&path).await.at(&path)? {
            return Ok(path);
        }
    }
    Err(Error::Io {
        path: dir.join(name),
        source: std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free file name"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn playlist_content_types() {
        assert!(is_playlist_type("application/vnd.apple.mpegurl"));
        assert!(is_playlist_type("application/x-mpegURL; charset=utf-8"));
        assert!(is_playlist_type("application/dash+xml"));
        assert!(!is_playlist_type("video/mp4"));
        assert!(!is_playlist_type("text/html"));
    }

    #[test]
    fn names_from_urls() {
        assert_eq!(
            stream_name(&url("https://x/movies/big-buck-bunny/master.m3u8")),
            "big-buck-bunny"
        );
        assert_eq!(stream_name(&url("https://x/v/My%20Talk.m3u8?t=1")), "My Talk");
        assert_eq!(stream_name(&url("https://x/a/b/manifest.mpd")), "b");
        assert_eq!(stream_name(&url("https://cdn.example/index.m3u8")), "cdn.example");
        assert_eq!(strip_media_extension("talk.mp4"), "talk");
        assert_eq!(strip_media_extension("talk.final"), "talk.final");
    }

    #[test]
    fn aes_round_trip() {
        use aes::cipher::BlockEncryptMut;
        let (key, iv) = ([7u8; 16], [9u8; 16]);
        let plain = b"segment bytes that are not a multiple of sixteen".to_vec();
        let mut buf = plain.clone();
        buf.resize(plain.len() + 16, 0);
        let enc_len = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .len();
        buf.truncate(enc_len);
        decrypt(&mut buf, &key, &iv).unwrap();
        assert_eq!(buf, plain);
        let mut wrong = vec![0u8; 32];
        assert!(decrypt(&mut wrong, &key, &iv).is_err());
    }

    #[test]
    fn remux_rules() {
        let track = |container, kind| Track { kind, container, init: None, segments: Vec::new() };
        assert!(needs_remux(&[track(Container::Ts, TrackKind::Muxed)]));
        assert!(!needs_remux(&[track(Container::Mp4, TrackKind::Muxed)]));
        assert!(needs_remux(&[
            track(Container::Mp4, TrackKind::Video),
            track(Container::Mp4, TrackKind::Audio)
        ]));
    }
}
