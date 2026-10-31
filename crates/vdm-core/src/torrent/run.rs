//! Torrents as downloads: where the torrent comes from, the work folder and
//! resume, progress reports, finishing, and seeding afterwards.
//!
//! Work folder `<name>.vdmpart/` (claimed like any part):
//! - `torrent.bencode`: the torrent (info dictionary and trackers), so a resume
//!   needs neither the original `.torrent` nor the magnet's peers again;
//! - `resume.bin`: which pieces are done;
//! - `data/`: the content as it downloads, moved out when complete.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Url;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use vdm_proto::SegmentProgress;

use super::metainfo::{self, Magnet, Metainfo, hex, parse_magnet, parse_torrent};
use super::session::{self, Config, Swarm};
use super::storage::{Storage, load_resume, save_resume};
use super::tracker::Event;
use super::wire::Bitfield;
use super::{new_peer_id, tracker};
use crate::download::{Completed, DownloadRequest, Downloader, Observer, Progress, Started};
use crate::error::{Error, IoContext, Result};
use crate::filename;
use crate::limiter::RateLimiter;
use crate::part::{PartMeta, Slot};
use crate::probe::ProbeInfo;

/// Largest `.torrent` file accepted.
const MAX_TORRENT_FILE: usize = 32 << 20;
const PROGRESS_EVERY: Duration = Duration::from_millis(500);
const SAVE_EVERY: Duration = Duration::from_secs(3);
/// Most cells in the piece map sent with progress.
const MAP_CELLS: u32 = 400;

/// A torrent link: `magnet:`, or an `http(s)://` or `file://` URL of a `.torrent` file.
pub fn is_torrent_url(url: &str) -> bool {
    if metainfo::is_magnet(url) {
        return true;
    }
    Url::parse(url.trim()).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https" | "file")
            && u.path().to_ascii_lowercase().ends_with(".torrent")
    })
}

/// A name to show for a torrent link before its details are known: the
/// magnet's `dn` (or its hash), or the `.torrent` file's name.
pub fn display_name(url: &str) -> String {
    if let Ok(m) = parse_magnet(url) {
        return m.name.unwrap_or_else(|| hex(&m.info_hash));
    }
    let file = Url::parse(url.trim())
        .ok()
        .and_then(|u| u.path_segments().and_then(|mut s| s.next_back().map(str::to_owned)))
        .map(|s| percent_encoding::percent_decode_str(&s).decode_utf8_lossy().into_owned())
        .unwrap_or_default();
    let stem =
        file.strip_suffix(".torrent").or_else(|| file.strip_suffix(".TORRENT")).unwrap_or(&file);
    filename::sanitize(stem)
}

/// A finished torrent that can be seeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    /// The torrent as cached in the work folder (info dictionary and trackers).
    torrent: Vec<u8>,
    /// Where the content ended up (it may have been numbered).
    pub content: PathBuf,
}

impl Seed {
    /// Seed `content` (the file or folder) of the `.torrent` in `torrent`.
    pub fn new(torrent: Vec<u8>, content: PathBuf) -> Self {
        Self { torrent, content }
    }
}

enum Source {
    File(Metainfo),
    Magnet(Magnet),
}

fn torrent_err(msg: impl Into<String>) -> Error {
    Error::Torrent(msg.into())
}

impl Downloader {
    /// Read the torrent behind `url`: parse a magnet, or fetch and parse a `.torrent`.
    async fn torrent_source(
        &self,
        url: &str,
        headers: &[(String, String)],
        cancel: &CancellationToken,
    ) -> Result<Source> {
        if metainfo::is_magnet(url) {
            return parse_magnet(url).map(Source::Magnet).map_err(torrent_err);
        }
        let parsed = Url::parse(url.trim()).map_err(|_| Error::InvalidUrl(url.to_owned()))?;
        let bytes = if parsed.scheme() == "file" {
            let path = parsed.to_file_path().map_err(|()| Error::InvalidUrl(url.to_owned()))?;
            tokio::fs::read(&path).await.at(&path)?
        } else {
            self.fetch_torrent_file(&parsed, headers, cancel).await?
        };
        if bytes.len() > MAX_TORRENT_FILE {
            return Err(torrent_err("the .torrent file is too large"));
        }
        parse_torrent(&bytes)
            .map(Source::File)
            .map_err(|e| torrent_err(format!("not a valid torrent: {e}")))
    }

    async fn fetch_torrent_file(
        &self,
        url: &Url,
        headers: &[(String, String)],
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        let mut map = crate::http::build_headers(headers)?;
        crate::auth::apply_login(&mut map, url, &self.options().logins);
        let fetch = async {
            let response =
                self.client().get(url.clone()).headers(map).send().await.map_err(Error::Network)?;
            let status = response.status();
            if !status.is_success() {
                return Err(Error::Status { status, retry_after: None });
            }
            response.bytes().await.map(|b| b.to_vec()).map_err(Error::Network)
        };
        tokio::select! {
            () = cancel.cancelled() => Err(Error::Cancelled { downloaded: 0 }),
            r = fetch => r,
        }
    }

    /// What a torrent link holds: its name and size, without downloading it.
    /// For magnets that's only what the link itself says.
    pub(crate) async fn probe_torrent(
        &self,
        url: &str,
        headers: &[(String, String)],
        cancel: &CancellationToken,
    ) -> Result<ProbeInfo> {
        let (name, size) = match self.torrent_source(url, headers, cancel).await? {
            Source::File(t) => (t.info.name.clone(), Some(t.info.total_length())),
            Source::Magnet(m) => (m.name.clone().unwrap_or_else(|| hex(&m.info_hash)), m.size),
        };
        Ok(ProbeInfo {
            final_url: url.to_owned(),
            size,
            resumable: true,
            etag: None,
            last_modified: None,
            content_type: Some("application/x-bittorrent".into()),
            file_name: name,
        })
    }

    pub(crate) async fn download_torrent<O: Observer>(
        &self,
        req: &DownloadRequest,
        observer: &mut O,
        cancel: &CancellationToken,
    ) -> Result<Completed> {
        // A name from an earlier run finds its work folder, whose cached copy
        // of the torrent means the source isn't needed again.
        let mut claimed = None;
        let mut torrent = None;
        if let Some(name) = &req.file_name {
            let (slot, claim) = self.claim_torrent_slot(req, &filename::sanitize(name)).await?;
            if slot.previous.is_some() {
                torrent = read_cache(&slot.part).await;
            }
            claimed = Some((slot, claim));
        }
        let mut initial_peers = Vec::new();
        let torrent = match torrent {
            Some(t) => t,
            None => match self.torrent_source(&req.url, &req.headers, cancel).await? {
                Source::File(t) => t,
                Source::Magnet(m) => {
                    initial_peers.clone_from(&m.peers);
                    self.magnet_metadata(&m, cancel).await?
                }
            },
        };
        let (slot, _claim) = match claimed {
            Some(sc) => sc,
            None => self.claim_torrent_slot(req, &torrent.info.name).await?,
        };
        if slot.previous.is_none() {
            fresh_work(&slot, &req.url).await?;
        }
        if read_cache(&slot.part).await.is_none() {
            write_cache(&slot.part, &torrent.info_bytes, &torrent.trackers).await?;
        }

        let info = Arc::new(torrent.info.clone());
        let total = info.total_length();
        let data_root = slot.part.join("data");
        let storage = Arc::new(Storage::new(&data_root, info.clone()));
        let resume_path = slot.part.join("resume.bin");
        let have = {
            let (path, hash, count) = (resume_path.clone(), torrent.info_hash, info.piece_count());
            let storage = storage.clone();
            let root = data_root.clone();
            tokio::task::spawn_blocking(move || {
                load_resume(&path, &hash, count).unwrap_or_else(|| {
                    // No record: check whatever is already on disk.
                    if root.exists() { storage.verify_all() } else { Bitfield::new(count) }
                })
            })
            .await
            .map_err(|e| torrent_err(e.to_string()))?
        };
        let resumed_from =
            (0..have.len()).filter(|&i| have.has(i)).map(|i| info.piece_size(i)).sum();
        observer.started(&Started {
            file_name: slot.name(),
            part_path: slot.part.clone(),
            size: Some(total),
            resumable: true,
            connections: 0,
            resumed_from,
        });
        tracing::info!(name = %info.name, hash = %hex(&torrent.info_hash), pieces = info.piece_count(), "torrent");

        if !have.is_complete() {
            self.transfer(
                &torrent,
                storage.clone(),
                have,
                &resume_path,
                &req.limits,
                initial_peers,
                observer,
                cancel,
            )
            .await?;
        }

        // Done: move the content next to the other downloads.
        let finish = storage.clone();
        tokio::task::spawn_blocking(move || finish.finish())
            .await
            .map_err(|e| torrent_err(e.to_string()))?
            .at(storage.content_path())?;
        let path = slot.finish_from(storage.content_path()).await?;
        let cache = tokio::fs::read(slot.part.join(CACHE_FILE)).await.unwrap_or_default();
        if let Err(err) = tokio::fs::remove_dir_all(&slot.part).await {
            tracing::warn!(path = %slot.part.display(), %err, "could not delete the work folder");
        }
        observer.segments(map_of(&Bitfield::full(info.piece_count()), &info), 0);
        observer.progress(Progress { downloaded: total, total: Some(total) });
        let seed = self
            .options()
            .torrent_upload
            .then(|| Arc::new(Seed { torrent: cache, content: path.clone() }));
        Ok(Completed { path, size: total, resumed_from, restarted: false, seed })
    }

    async fn claim_torrent_slot(
        &self,
        req: &DownloadRequest,
        base: &str,
    ) -> Result<(Slot, crate::part::Claim)> {
        tokio::fs::create_dir_all(&req.out_dir).await.at(&req.out_dir)?;
        Slot::claim(&req.out_dir, base, &req.url, self.claims()).await
    }

    /// Fetch a magnet's info dictionary from peers found by its trackers and the link.
    async fn magnet_metadata(&self, m: &Magnet, cancel: &CancellationToken) -> Result<Metainfo> {
        if m.trackers.is_empty() && m.peers.is_empty() {
            return Err(torrent_err(
                "this magnet link lists no trackers, and Vibe can't search the DHT yet; use the .torrent file instead",
            ));
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for p in &m.peers {
            let _ = tx.send(*p);
        }
        let peer_id = new_peer_id();
        let stop = cancel.child_token();
        let mut lookups = JoinSet::new();
        for url in m.trackers.clone() {
            let (tx, client, stop, hash) =
                (tx.clone(), self.client().clone(), stop.clone(), m.info_hash);
            lookups.spawn(async move {
                let a = tracker::Announce {
                    info_hash: hash,
                    peer_id,
                    port: 0,
                    uploaded: 0,
                    downloaded: 0,
                    left: 1,
                    event: Event::Started,
                };
                loop {
                    let wait = tokio::select! {
                        () = stop.cancelled() => return,
                        r = tracker::announce(&client, &url, &a) => match r {
                            Ok(reply) => {
                                for p in reply.peers {
                                    let _ = tx.send(p);
                                }
                                Duration::from_secs(60)
                            }
                            Err(_) => Duration::from_secs(30),
                        },
                    };
                    tokio::select! {
                        () = stop.cancelled() => return,
                        () = tokio::time::sleep(wait) => {}
                    }
                }
            });
        }
        drop(tx);
        tracing::info!(hash = %hex(&m.info_hash), "fetching the torrent's details from peers");
        let result = session::fetch_metadata(m.info_hash, peer_id, rx, cancel).await;
        stop.cancel();
        lookups.abort_all();
        let info_bytes = match result {
            Ok(bytes) => bytes,
            Err(_) if cancel.is_cancelled() => return Err(Error::Cancelled { downloaded: 0 }),
            Err(e) => return Err(torrent_err(e)),
        };
        let info = metainfo::parse_info(&info_bytes).map_err(torrent_err)?;
        Ok(Metainfo { info_hash: m.info_hash, info, info_bytes, trackers: m.trackers.clone() })
    }

    /// Download the missing pieces.
    #[allow(clippy::too_many_arguments)]
    async fn transfer<O: Observer>(
        &self,
        torrent: &Metainfo,
        storage: Arc<Storage>,
        have: Bitfield,
        resume_path: &Path,
        limits: &[Arc<RateLimiter>],
        initial_peers: Vec<SocketAddr>,
        observer: &mut O,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let upload = self.options().torrent_upload;
        let listener = if upload { listen(self.options().torrent_port).await } else { None };
        let port = listener.as_ref().and_then(|l| l.local_addr().ok()).map_or(0, |a| a.port());
        let config = Config {
            info_hash: torrent.info_hash,
            peer_id: new_peer_id(),
            upload,
            port,
            max_peers: self.options().torrent_max_peers.max(1),
            private: torrent.info.private,
        };
        let info = Arc::new(torrent.info.clone());
        let swarm = Swarm::new(
            config,
            info.clone(),
            Arc::new(torrent.info_bytes.clone()),
            storage,
            have,
            limits.to_vec(),
            cancel.child_token(),
        );
        swarm.add_peers(initial_peers);
        let mut tasks = JoinSet::new();
        session::spawn(
            &swarm,
            self.client().clone(),
            torrent.trackers.clone(),
            listener,
            &mut tasks,
        );

        let total = info.total_length();
        let mut tick = tokio::time::interval(PROGRESS_EVERY);
        let mut last_save = Instant::now();
        let outcome = loop {
            tokio::select! {
                () = cancel.cancelled() => break Err(()),
                () = swarm.cancel.cancelled() => break Err(()),
                _ = tick.tick() => {}
            }
            let snap = swarm.snapshot();
            if let Some(have) = &snap.have {
                let peers = u8::try_from(snap.peers).unwrap_or(u8::MAX);
                observer.segments(map_of(have, &info), peers);
            }
            observer
                .progress(Progress { downloaded: snap.downloaded.min(total), total: Some(total) });
            if last_save.elapsed() >= SAVE_EVERY || snap.complete {
                save(&swarm, resume_path).await;
                last_save = Instant::now();
            }
            if snap.complete {
                break Ok(());
            }
        };
        swarm.cancel.cancel();
        tasks.abort_all();
        save(&swarm, resume_path).await;
        let event = if outcome.is_ok() { Event::Completed } else { Event::Stopped };
        session::announce_event(&swarm, self.client(), &torrent.trackers, event).await;
        if let Some(err) = swarm.error() {
            return Err(torrent_err(err));
        }
        match outcome {
            Ok(()) => Ok(()),
            Err(()) => Err(Error::Cancelled { downloaded: swarm.snapshot().downloaded }),
        }
    }

    /// Seed a finished torrent until it has uploaded [`Options::torrent_seed_ratio`]
    /// times its size, or `cancel` fires. Returns the bytes uploaded.
    pub async fn seed(
        &self,
        seed: &Seed,
        limits: &[Arc<RateLimiter>],
        cancel: &CancellationToken,
    ) -> Result<u64> {
        let torrent = parse_torrent(&seed.torrent).map_err(torrent_err)?;
        let info = Arc::new(torrent.info.clone());
        let storage = Arc::new(Storage::at(seed.content.clone(), info.clone()));
        let present = {
            let s = storage.clone();
            tokio::task::spawn_blocking(move || s.files_present()).await.unwrap_or(false)
        };
        if !present {
            return Err(torrent_err(
                "the downloaded files were moved or changed, so they can't be seeded",
            ));
        }
        let listener = listen(self.options().torrent_port).await;
        let port = listener.as_ref().and_then(|l| l.local_addr().ok()).map_or(0, |a| a.port());
        let config = Config {
            info_hash: torrent.info_hash,
            peer_id: new_peer_id(),
            upload: true,
            port,
            max_peers: self.options().torrent_max_peers.max(1),
            private: info.private,
        };
        let swarm = Swarm::new(
            config,
            info.clone(),
            Arc::new(torrent.info_bytes.clone()),
            storage,
            Bitfield::full(info.piece_count()),
            limits.to_vec(),
            cancel.child_token(),
        );
        let mut tasks = JoinSet::new();
        session::spawn(
            &swarm,
            self.client().clone(),
            torrent.trackers.clone(),
            listener,
            &mut tasks,
        );
        let target = self
            .options()
            .torrent_seed_ratio
            .map(|ratio| (info.total_length() as f64 * ratio.max(0.0)) as u64);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {}
            }
            if let Some(target) = target
                && swarm.snapshot().uploaded >= target
            {
                tracing::info!(name = %info.name, "seeded to the set ratio; stopping");
                break;
            }
        }
        swarm.cancel.cancel();
        tasks.abort_all();
        session::announce_event(&swarm, self.client(), &torrent.trackers, Event::Stopped).await;
        Ok(swarm.snapshot().uploaded)
    }
}

/// Accept peers on `port` (0: any free port), on all interfaces.
async fn listen(port: u16) -> Option<TcpListener> {
    match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => Some(l),
        Err(err) => {
            tracing::warn!(port, %err, "can't accept torrent peers");
            None
        }
    }
}

async fn save(swarm: &Swarm, path: &Path) {
    if let Some(have) = swarm.take_dirty() {
        let (path, hash) = (path.to_owned(), swarm.config.info_hash);
        let result = tokio::task::spawn_blocking(move || save_resume(&path, &hash, &have)).await;
        if let Ok(Err(err)) = result {
            tracing::warn!(%err, "could not save torrent progress");
        }
    }
}

/// A fresh start: clear any leftover work folder and record which URL it's for.
async fn fresh_work(slot: &Slot, url: &str) -> Result<()> {
    if tokio::fs::try_exists(&slot.part).await.at(&slot.part)? {
        tokio::fs::remove_dir_all(&slot.part).await.at(&slot.part)?;
    }
    tokio::fs::create_dir_all(&slot.part).await.at(&slot.part)?;
    slot.write_meta(&PartMeta::new(url, None, None, None)).await
}

const CACHE_FILE: &str = "torrent.bencode";

/// A `.torrent` made of the info dictionary (byte for byte) and the trackers.
fn cache_bytes(info_bytes: &[u8], trackers: &[String]) -> Vec<u8> {
    use super::bencode::{Value, bytes};
    let tiers =
        Value::List(trackers.iter().map(|t| Value::List(vec![bytes(t.as_str())])).collect());
    let mut out = b"d13:announce-list".to_vec();
    out.extend(tiers.encode());
    out.extend_from_slice(b"4:info");
    out.extend_from_slice(info_bytes);
    out.push(b'e');
    out
}

async fn write_cache(work: &Path, info_bytes: &[u8], trackers: &[String]) -> Result<()> {
    tokio::fs::create_dir_all(work).await.at(work)?;
    let path = work.join(CACHE_FILE);
    tokio::fs::write(&path, cache_bytes(info_bytes, trackers)).await.at(&path)
}

async fn read_cache(work: &Path) -> Option<Metainfo> {
    let bytes = tokio::fs::read(work.join(CACHE_FILE)).await.ok()?;
    parse_torrent(&bytes).ok()
}

/// The piece map as segments for the progress bar: runs of done and missing
/// pieces, or for big torrents `MAP_CELLS` cells, each as full as its share.
pub(crate) fn map_of(have: &Bitfield, info: &metainfo::Info) -> Vec<SegmentProgress> {
    let count = have.len();
    let total = info.total_length();
    if count == 0 {
        return Vec::new();
    }
    let cells = count.min(MAP_CELLS);
    let mut out = Vec::with_capacity(cells as usize);
    for c in 0..cells {
        let first = (u64::from(c) * u64::from(count) / u64::from(cells)) as u32;
        let last = (u64::from(c + 1) * u64::from(count) / u64::from(cells)) as u32;
        let start = u64::from(first) * info.piece_length;
        let end = (u64::from(last) * info.piece_length).min(total);
        let done: u64 = (first..last).filter(|&i| have.has(i)).map(|i| info.piece_size(i)).sum();
        out.push(SegmentProgress { start, end, pos: start + done });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torrent_urls() {
        assert!(is_torrent_url("magnet:?xt=urn:btih:abc"));
        assert!(is_torrent_url("https://example.com/files/ubuntu.iso.torrent?x=1"));
        assert!(is_torrent_url("file:///C:/Users/me/Downloads/a.TORRENT"));
        assert!(!is_torrent_url("https://example.com/a.torrent.zip"));
        assert!(!is_torrent_url("ftp://example.com/a.torrent"));
        assert!(!is_torrent_url("https://example.com/"));
    }

    #[test]
    fn display_names() {
        assert_eq!(
            display_name("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=My+Film"),
            "My Film"
        );
        assert_eq!(
            display_name("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a"),
            "c12fe1c06bba254a9dc9f519b335aa7c1367a88a"
        );
        assert_eq!(display_name("https://x/dl/ubuntu%2024.iso.torrent?k=1"), "ubuntu 24.iso");
    }

    #[test]
    fn the_cache_is_a_torrent_with_the_same_hash() {
        let original = metainfo::make_torrent("x", &[("x", b"hello world")], 4, "udp://t:1");
        let t = parse_torrent(&original).unwrap();
        let cached =
            parse_torrent(&cache_bytes(&t.info_bytes, &["http://a/b".into(), "udp://t:1".into()]))
                .unwrap();
        assert_eq!(cached.info_hash, t.info_hash);
        assert_eq!(cached.trackers, ["http://a/b", "udp://t:1"]);
    }

    #[test]
    fn the_piece_map_has_one_cell_per_piece_or_fewer() {
        let data = vec![0u8; 10 * 4];
        let info = parse_torrent(&metainfo::make_torrent("x", &[("x", &data)], 4, "udp://t:1"))
            .unwrap()
            .info;
        let mut have = Bitfield::new(10);
        have.set(0);
        have.set(9);
        let map = map_of(&have, &info);
        assert_eq!(map.len(), 10);
        assert_eq!(map[0], SegmentProgress { start: 0, end: 4, pos: 4 });
        assert_eq!(map[1], SegmentProgress { start: 4, end: 8, pos: 4 });
        assert_eq!(map[9].end, 40);
    }
}
