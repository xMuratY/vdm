//! One torrent's swarm: connections to peers, the request pipeline, uploads,
//! the choker, tracker announces, and fetching the info dictionary for magnets.
//!
//! Each peer connection is a task; they share the [`Swarm`] (picker, peer
//! table, counters) behind one mutex that is never held across an await.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::metainfo::{Hash, Info, sha1};
use super::picker::{Arrival, PeerKey, Picker};
use super::storage::Storage;
use super::tracker::{self, Announce, Event};
use super::wire::{
    self, BLOCK, Bitfield, ExtHandshake, Handshake, METADATA_PIECE, Message, MetadataMsg,
    UT_METADATA, UT_PEX,
};
use crate::limiter::{RateLimiter, acquire_all};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Requests kept outstanding per peer.
const PIPELINE: usize = 64;
/// Drop a peer that sent no block for this long while we waited on it.
const SNUB_TIMEOUT: Duration = Duration::from_secs(45);
/// Drop a connection that's been silent this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(180);
const KEEPALIVE: Duration = Duration::from_secs(90);
/// Requests a peer may queue with us.
const MAX_UPLOAD_QUEUE: usize = 256;
/// Peers uploaded to at once (plus one optimistic unchoke).
const UPLOAD_SLOTS: usize = 3;
const CHOKE_INTERVAL: Duration = Duration::from_secs(10);
/// Don't retry a peer address that failed for this long.
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
const MAX_CANDIDATES: usize = 2000;

/// Settings for a swarm.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub info_hash: Hash,
    pub peer_id: [u8; 20],
    /// Upload to peers who ask (otherwise never unchoke anyone).
    pub upload: bool,
    /// The port we accept connections on, announced to trackers; 0 if none.
    pub port: u16,
    pub max_peers: usize,
    /// No peer exchange (private torrents).
    pub private: bool,
}

#[derive(Debug, Default)]
struct PeerStat {
    /// The peer wants data from us.
    interested: bool,
    /// The choker lets this peer download from us.
    unchoked: bool,
    received: u64,
    sent: u64,
    /// `received`/`sent` at the last choke round, for rates.
    last_received: u64,
    last_sent: u64,
    rate: u64,
}

struct State {
    picker: Picker,
    peers: HashMap<PeerKey, PeerStat>,
    connected: HashSet<SocketAddr>,
    connecting: usize,
    candidates: VecDeque<SocketAddr>,
    known: HashSet<SocketAddr>,
    failed: HashMap<SocketAddr, Instant>,
    /// Pieces completed, in order, so each peer can announce the new ones.
    haves: Vec<u32>,
    received: u64,
    uploaded: u64,
    next_key: PeerKey,
    error: Option<String>,
    /// The piece set changed since the resume file was written.
    dirty: bool,
}

/// What a swarm is doing right now, for progress reports.
#[derive(Debug, Clone, Default)]
pub(crate) struct Snapshot {
    pub downloaded: u64,
    pub uploaded: u64,
    pub peers: usize,
    pub have: Option<Bitfield>,
    pub complete: bool,
}

pub(crate) struct Swarm {
    pub config: Config,
    pub info: Arc<Info>,
    pub info_bytes: Arc<Vec<u8>>,
    pub storage: Arc<Storage>,
    pub limits: Vec<Arc<RateLimiter>>,
    state: Mutex<State>,
    /// Woken when a piece completes or the swarm should stop.
    notify: Notify,
    /// Woken when new peer addresses arrive.
    found: Notify,
    pub cancel: CancellationToken,
}

impl Swarm {
    pub fn new(
        config: Config,
        info: Arc<Info>,
        info_bytes: Arc<Vec<u8>>,
        storage: Arc<Storage>,
        have: Bitfield,
        limits: Vec<Arc<RateLimiter>>,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        let state = State {
            picker: Picker::new(&info, have),
            peers: HashMap::new(),
            connected: HashSet::new(),
            connecting: 0,
            candidates: VecDeque::new(),
            known: HashSet::new(),
            failed: HashMap::new(),
            haves: Vec::new(),
            received: 0,
            uploaded: 0,
            next_key: 1,
            error: None,
            dirty: false,
        };
        Arc::new(Self {
            config,
            info,
            info_bytes,
            storage,
            limits,
            state: Mutex::new(state),
            notify: Notify::new(),
            found: Notify::new(),
            cancel,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add_peers(&self, addrs: impl IntoIterator<Item = SocketAddr>) {
        let mut st = self.lock();
        let mut added = false;
        for addr in addrs {
            if st.known.len() < MAX_CANDIDATES && st.known.insert(addr) {
                st.candidates.push_back(addr);
                added = true;
            }
        }
        drop(st);
        if added {
            self.found.notify_one();
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let st = self.lock();
        Snapshot {
            downloaded: st.picker.downloaded(),
            uploaded: st.uploaded,
            peers: st.peers.len(),
            have: Some(st.picker.have().clone()),
            complete: st.picker.is_complete(),
        }
    }

    pub fn error(&self) -> Option<String> {
        self.lock().error.clone()
    }

    /// The piece set, if it changed since the last call.
    pub fn take_dirty(&self) -> Option<Bitfield> {
        let mut st = self.lock();
        std::mem::take(&mut st.dirty).then(|| st.picker.have().clone())
    }

    fn fail(&self, err: String) {
        self.lock().error.get_or_insert(err);
        self.cancel.cancel();
    }

    fn announce_numbers(&self, event: Event) -> Announce {
        let st = self.lock();
        Announce {
            info_hash: self.config.info_hash,
            peer_id: self.config.peer_id,
            port: self.config.port,
            uploaded: st.uploaded,
            downloaded: st.received,
            left: st.picker.left(),
            event,
        }
    }
}

// ------------------------------------------------------------ running

/// Run the swarm's background work (trackers, connecting, accepting, choking)
/// until its cancel token fires. The caller watches progress and decides when
/// to stop.
pub(crate) fn spawn(
    swarm: &Arc<Swarm>,
    client: reqwest::Client,
    trackers: Vec<String>,
    listener: Option<TcpListener>,
    tasks: &mut JoinSet<()>,
) {
    for url in trackers {
        tasks.spawn(tracker_loop(swarm.clone(), client.clone(), url));
    }
    tasks.spawn(connector(swarm.clone()));
    if swarm.config.upload {
        tasks.spawn(choker(swarm.clone()));
    }
    if let Some(listener) = listener {
        tasks.spawn(acceptor(swarm.clone(), listener));
    }
}

/// Tell the trackers we finished or are leaving (best effort, for a few seconds).
pub(crate) async fn announce_event(
    swarm: &Swarm,
    client: &reqwest::Client,
    trackers: &[String],
    event: Event,
) {
    let a = swarm.announce_numbers(event);
    let mut calls = JoinSet::new();
    for url in trackers {
        let (client, url, a) = (client.clone(), url.clone(), a.clone());
        calls.spawn(async move { tracker::announce(&client, &url, &a).await });
    }
    let _ = tokio::time::timeout(Duration::from_secs(4), calls.join_all()).await;
}

async fn tracker_loop(swarm: Arc<Swarm>, client: reqwest::Client, url: String) {
    let mut event = Event::Started;
    let mut failures = 0u32;
    loop {
        let a = swarm.announce_numbers(event);
        let wait = tokio::select! {
            () = swarm.cancel.cancelled() => return,
            r = tracker::announce(&client, &url, &a) => match r {
                Ok(reply) => {
                    tracing::debug!(%url, peers = reply.peers.len(), "tracker replied");
                    swarm.add_peers(reply.peers);
                    event = Event::None;
                    failures = 0;
                    reply.interval
                }
                Err(err) => {
                    tracing::debug!(%url, %err, "tracker failed");
                    failures += 1;
                    Duration::from_secs(30 * 2u64.pow(failures.min(6)))
                }
            },
        };
        tokio::select! {
            () = swarm.cancel.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

async fn connector(swarm: Arc<Swarm>) {
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            () = swarm.cancel.cancelled() => return,
            _ = tick.tick() => {}
            () = swarm.found.notified() => {}
        }
        // Download-only swarms that are done connect to no one.
        if swarm.snapshot_complete() && !swarm.config.upload {
            continue;
        }
        loop {
            let next = {
                let mut st = swarm.lock();
                if st.peers.len() + st.connecting >= swarm.config.max_peers {
                    None
                } else {
                    let now = Instant::now();
                    let mut found = None;
                    while let Some(addr) = st.candidates.pop_front() {
                        let recently_failed = st
                            .failed
                            .get(&addr)
                            .is_some_and(|t| now.duration_since(*t) < RETRY_AFTER);
                        if !st.connected.contains(&addr) && !recently_failed {
                            found = Some(addr);
                            break;
                        }
                    }
                    if found.is_some() {
                        st.connecting += 1;
                    }
                    found
                }
            };
            let Some(addr) = next else { break };
            let swarm = swarm.clone();
            tokio::spawn(async move {
                let result = connect_out(&swarm, addr).await;
                {
                    let mut st = swarm.lock();
                    st.connecting -= 1;
                }
                match result {
                    Ok((stream, hs)) => run_peer(swarm.clone(), stream, addr, hs).await,
                    Err(err) => tracing::trace!(%addr, %err, "connect failed"),
                }
                let mut st = swarm.lock();
                st.failed.insert(addr, Instant::now());
                // It may be worth another try later.
                st.known.remove(&addr);
            });
        }
    }
}

impl Swarm {
    fn snapshot_complete(&self) -> bool {
        self.lock().picker.is_complete()
    }
}

async fn connect_out(swarm: &Swarm, addr: SocketAddr) -> Result<(TcpStream, Handshake), String> {
    let work = async {
        let mut stream = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        stream.set_nodelay(true).ok();
        let ours = Handshake {
            info_hash: swarm.config.info_hash,
            peer_id: swarm.config.peer_id,
            extensions: true,
        };
        wire::write_handshake(&mut stream, &ours).await.map_err(|e| e.to_string())?;
        let theirs = wire::read_handshake(&mut stream).await.map_err(|e| e.to_string())?;
        if theirs.info_hash != swarm.config.info_hash {
            return Err("the peer is on another torrent".into());
        }
        if theirs.peer_id == swarm.config.peer_id {
            return Err("connected to ourselves".into());
        }
        Ok((stream, theirs))
    };
    tokio::time::timeout(CONNECT_TIMEOUT, work).await.map_err(|_| "timed out".to_owned())?
}

async fn acceptor(swarm: Arc<Swarm>, listener: TcpListener) {
    loop {
        let accepted = tokio::select! {
            () = swarm.cancel.cancelled() => return,
            r = listener.accept() => r,
        };
        let Ok((mut stream, addr)) = accepted else { continue };
        {
            let st = swarm.lock();
            if st.peers.len() >= swarm.config.max_peers || st.connected.contains(&addr) {
                continue;
            }
        }
        let swarm = swarm.clone();
        tokio::spawn(async move {
            let work = async {
                let theirs = wire::read_handshake(&mut stream).await.ok()?;
                if theirs.info_hash != swarm.config.info_hash
                    || theirs.peer_id == swarm.config.peer_id
                {
                    return None;
                }
                let ours = Handshake {
                    info_hash: swarm.config.info_hash,
                    peer_id: swarm.config.peer_id,
                    extensions: true,
                };
                wire::write_handshake(&mut stream, &ours).await.ok()?;
                Some(theirs)
            };
            if let Ok(Some(hs)) = tokio::time::timeout(CONNECT_TIMEOUT, work).await {
                stream.set_nodelay(true).ok();
                run_peer(swarm, stream, addr, hs).await;
            }
        });
    }
}

/// Every 10 s, let the peers that gave us the most (or, when seeding, took the
/// most) download from us, plus one chosen at random.
async fn choker(swarm: Arc<Swarm>) {
    let mut tick = tokio::time::interval(CHOKE_INTERVAL);
    let mut round = 0u64;
    loop {
        tokio::select! {
            () = swarm.cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        round += 1;
        let mut st = swarm.lock();
        let seeding = st.picker.is_complete();
        for p in st.peers.values_mut() {
            p.rate = if seeding { p.sent - p.last_sent } else { p.received - p.last_received };
            p.last_sent = p.sent;
            p.last_received = p.received;
        }
        let mut interested: Vec<(PeerKey, u64)> =
            st.peers.iter().filter(|(_, p)| p.interested).map(|(k, p)| (*k, p.rate)).collect();
        interested.sort_by_key(|p| std::cmp::Reverse(p.1));
        let mut chosen: HashSet<PeerKey> =
            interested.iter().take(UPLOAD_SLOTS).map(|(k, _)| *k).collect();
        // The optimistic slot changes every third round.
        let rest: Vec<PeerKey> = interested.iter().skip(UPLOAD_SLOTS).map(|(k, _)| *k).collect();
        if !rest.is_empty() {
            let pick = ((super::random_u64() ^ (round / 3)) % rest.len() as u64) as usize;
            chosen.insert(rest[pick]);
        }
        for (k, p) in st.peers.iter_mut() {
            p.unchoked = chosen.contains(k);
        }
        drop(st);
        swarm.notify.notify_waiters();
    }
}

// ------------------------------------------------------------ one peer

struct Conn {
    key: PeerKey,
    stream: TcpStream,
    has: Bitfield,
    ext: ExtHandshake,
    peer_choking: bool,
    am_interested: bool,
    am_choking: bool,
    /// Blocks asked of this peer and not yet received.
    pending: HashSet<(u32, u32)>,
    uploads: VecDeque<(u32, u32, u32)>,
    /// How many of the swarm's completed pieces we've told this peer about.
    haves_sent: usize,
    out: Vec<u8>,
    last_block: Instant,
    last_heard: Instant,
    last_sent: Instant,
}

async fn run_peer(swarm: Arc<Swarm>, stream: TcpStream, addr: SocketAddr, hs: Handshake) {
    let pieces = swarm.info.piece_count();
    let key = {
        let mut st = swarm.lock();
        if !st.connected.insert(addr) {
            return;
        }
        let key = st.next_key;
        st.next_key += 1;
        st.peers.insert(key, PeerStat::default());
        // Anything the swarm completes from now on is announced to this peer.
        key
    };
    let now = Instant::now();
    let mut c = Conn {
        key,
        stream,
        has: Bitfield::new(pieces),
        ext: ExtHandshake::default(),
        peer_choking: true,
        am_interested: false,
        am_choking: true,
        pending: HashSet::new(),
        uploads: VecDeque::new(),
        haves_sent: 0,
        out: Vec::new(),
        last_block: now,
        last_heard: now,
        last_sent: now,
    };
    let result = peer_loop(&swarm, &mut c, hs.extensions).await;
    if let Err(err) = result {
        tracing::trace!(%addr, %err, "peer closed");
    }
    let mut st = swarm.lock();
    st.picker.peer_left(key, &c.has);
    st.peers.remove(&key);
    st.connected.remove(&addr);
}

async fn peer_loop(swarm: &Arc<Swarm>, c: &mut Conn, extensions: bool) -> Result<(), String> {
    if extensions {
        let size = Some(swarm.info_bytes.len());
        c.out.extend(
            wire::ext_handshake(size, Some(swarm.config.port).filter(|&p| p != 0)).encode(),
        );
    }
    {
        let st = swarm.lock();
        let have = st.picker.have();
        if have.count() > 0 {
            c.out.extend(Message::Bitfield(have.bytes().to_vec()).encode());
        }
        c.haves_sent = st.haves.len();
    }
    flush(c).await?;

    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        buf.reserve(32 * 1024);
        tokio::select! {
            () = swarm.cancel.cancelled() => return Ok(()),
            r = c.stream.read_buf(&mut buf) => {
                let n = r.map_err(|e| e.to_string())?;
                if n == 0 {
                    return Err("closed by the peer".into());
                }
                c.last_heard = Instant::now();
                while let Some(msg) = wire::take_frame(&mut buf)? {
                    handle(swarm, c, msg).await?;
                }
            }
            () = swarm.notify.notified() => {}
            // Uploads still queued: go round again (reads stay serviced, since
            // select picks among ready branches at random).
            () = std::future::ready(()), if !c.uploads.is_empty() && !c.am_choking => {}
            _ = tick.tick() => {
                let now = Instant::now();
                if !c.pending.is_empty() && now.duration_since(c.last_block) > SNUB_TIMEOUT {
                    return Err("the peer stopped sending".into());
                }
                if now.duration_since(c.last_heard) > IDLE_TIMEOUT {
                    return Err("the peer went quiet".into());
                }
                if now.duration_since(c.last_sent) > KEEPALIVE {
                    c.out.extend(Message::KeepAlive.encode());
                }
                // Done, not uploading, and the peer can't give us anything: hang up.
                let done = swarm.snapshot_complete();
                if done && !swarm.config.upload {
                    return Ok(());
                }
            }
        }
        pump(swarm, c).await?;
    }
}

async fn flush(c: &mut Conn) -> Result<(), String> {
    if !c.out.is_empty() {
        c.stream.write_all(&c.out).await.map_err(|e| e.to_string())?;
        c.out.clear();
        c.last_sent = Instant::now();
    }
    Ok(())
}

async fn handle(swarm: &Arc<Swarm>, c: &mut Conn, msg: Message) -> Result<(), String> {
    match msg {
        Message::KeepAlive | Message::Port(_) | Message::Unknown(_) => {}
        Message::Choke => {
            c.peer_choking = true;
            let mut st = swarm.lock();
            for (piece, begin) in c.pending.drain() {
                st.picker.release(c.key, piece, begin);
            }
        }
        Message::Unchoke => c.peer_choking = false,
        Message::Interested => set_interest(swarm, c.key, true),
        Message::NotInterested => set_interest(swarm, c.key, false),
        Message::Have(i) => {
            if i < c.has.len() && !c.has.has(i) {
                c.has.set(i);
                swarm.lock().picker.peer_has(i);
            }
        }
        Message::Bitfield(bits) => {
            let has = Bitfield::from_bytes(&bits, c.has.len()).ok_or("bad bitfield")?;
            let mut st = swarm.lock();
            // Replaces anything announced so far (normally nothing).
            st.picker.peer_left(c.key, &c.has);
            st.picker.peer_joined(&has);
            c.has = has;
        }
        Message::Request { index, begin, length } => {
            let ok = !c.am_choking
                && length > 0
                && length <= 2 * BLOCK
                && swarm.lock().picker.have().has(index)
                && u64::from(begin) + u64::from(length) <= swarm.info.piece_size(index);
            if ok && c.uploads.len() < MAX_UPLOAD_QUEUE {
                c.uploads.push_back((index, begin, length));
            }
        }
        Message::Cancel { index, begin, length } => {
            c.uploads.retain(|r| *r != (index, begin, length))
        }
        Message::Piece { index, begin, data } => {
            receive_block(swarm, c, index, begin, data).await?
        }
        Message::Extended { id, payload } => extended(swarm, c, id, &payload),
    }
    Ok(())
}

/// A peer said whether it wants data from us. When uploading and a slot is
/// free, it's unchoked right away rather than at the next choke round.
fn set_interest(swarm: &Swarm, key: PeerKey, interested: bool) {
    let mut st = swarm.lock();
    let unchoked = st.peers.values().filter(|p| p.unchoked).count();
    let Some(p) = st.peers.get_mut(&key) else { return };
    p.interested = interested;
    if !interested {
        p.unchoked = false;
    } else if swarm.config.upload && unchoked <= UPLOAD_SLOTS {
        p.unchoked = true;
    }
}

async fn receive_block(
    swarm: &Arc<Swarm>,
    c: &mut Conn,
    index: u32,
    begin: u32,
    data: Vec<u8>,
) -> Result<(), String> {
    if !c.pending.remove(&(index, begin)) {
        return Ok(()); // unasked, or after we gave up on it
    }
    c.last_block = Instant::now();
    let len = data.len() as u64;
    acquire_all(&swarm.limits, len).await;
    let arrival = {
        let mut st = swarm.lock();
        st.received += len;
        if let Some(p) = st.peers.get_mut(&c.key) {
            p.received += len;
        }
        st.picker.arrived(c.key, index, begin)
    };
    if arrival == Arrival::Unwanted {
        return Ok(());
    }
    let storage = swarm.storage.clone();
    let written = tokio::task::spawn_blocking(move || storage.write(index, begin, &data))
        .await
        .map_err(|e| e.to_string())?;
    if let Err(err) = written {
        swarm.fail(format!("can't write the download: {err}"));
        return Err(err.to_string());
    }
    if arrival == Arrival::PieceDone {
        let storage = swarm.storage.clone();
        let ok = tokio::task::spawn_blocking(move || storage.verify(index)).await.unwrap_or(false);
        let mut st = swarm.lock();
        st.picker.checked(index, ok);
        if ok {
            st.haves.push(index);
            st.dirty = true;
            drop(st);
            swarm.notify.notify_waiters();
        } else {
            tracing::debug!(piece = index, "piece failed its check; asking again");
        }
    }
    Ok(())
}

fn extended(swarm: &Swarm, c: &mut Conn, id: u8, payload: &[u8]) {
    match id {
        0 => {
            if let Some(ext) = wire::parse_ext_handshake(payload) {
                c.ext = ext;
            }
        }
        UT_METADATA => {
            // Serve our info dictionary to peers that ask (magnet users).
            let (Some(their_id), Some(MetadataMsg::Request(piece))) =
                (c.ext.ut_metadata, MetadataMsg::parse(payload))
            else {
                return;
            };
            let start = piece as usize * METADATA_PIECE;
            let reply = if start < swarm.info_bytes.len() {
                let end = (start + METADATA_PIECE).min(swarm.info_bytes.len());
                MetadataMsg::Data {
                    piece,
                    total_size: swarm.info_bytes.len(),
                    data: swarm.info_bytes[start..end].to_vec(),
                }
            } else {
                MetadataMsg::Reject(piece)
            };
            c.out.extend(reply.to_message(their_id).encode());
        }
        UT_PEX if !swarm.config.private => swarm.add_peers(wire::parse_pex(payload)),
        _ => {}
    }
}

/// Send what's due: new HAVEs, interest and choke changes, requests, uploads.
async fn pump(swarm: &Arc<Swarm>, c: &mut Conn) -> Result<(), String> {
    let (wanted, unchoke) = {
        let mut st = swarm.lock();
        for &piece in &st.haves[c.haves_sent..] {
            if !c.has.has(piece) {
                c.out.extend(Message::Have(piece).encode());
            }
        }
        c.haves_sent = st.haves.len();
        let unchoke = swarm.config.upload && st.peers.get(&c.key).is_some_and(|p| p.unchoked);
        let interested = st.picker.wants_from(&c.has);
        let mut wanted = Vec::new();
        if interested && !c.peer_choking {
            let room =
                PIPELINE.min(c.ext.reqq.unwrap_or(250) as usize).saturating_sub(c.pending.len());
            if room > 0 {
                wanted = st.picker.pick(c.key, &c.has, room);
            }
        }
        if interested != c.am_interested {
            c.am_interested = interested;
            c.out.extend(
                if interested { Message::Interested } else { Message::NotInterested }.encode(),
            );
        }
        (wanted, unchoke)
    };
    for b in wanted {
        c.pending.insert((b.piece, b.begin));
        c.out
            .extend(Message::Request { index: b.piece, begin: b.begin, length: b.length }.encode());
    }
    if unchoke == c.am_choking {
        c.am_choking = !unchoke;
        c.out.extend(if unchoke { Message::Unchoke } else { Message::Choke }.encode());
        if !unchoke {
            c.uploads.clear();
        }
    }
    flush(c).await?;

    // Uploads: a few blocks per round, so downloading isn't starved.
    let mut sent = 0;
    while sent < 4 && !c.am_choking {
        let Some((index, begin, length)) = c.uploads.pop_front() else { break };
        let storage = swarm.storage.clone();
        let data = tokio::task::spawn_blocking(move || storage.read(index, begin, length))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        c.out.extend(Message::Piece { index, begin, data }.encode());
        flush(c).await?;
        let len = u64::from(length);
        let mut st = swarm.lock();
        st.uploaded += len;
        if let Some(p) = st.peers.get_mut(&c.key) {
            p.sent += len;
        }
        sent += 1;
    }
    Ok(())
}

// ------------------------------------------------------------ magnet metadata

/// Fetch a torrent's info dictionary from peers (BEP 9), checked against its hash.
/// Peer addresses arrive on `peers` (from trackers, the link, or DHT).
pub(crate) async fn fetch_metadata(
    info_hash: Hash,
    peer_id: [u8; 20],
    mut peers: tokio::sync::mpsc::UnboundedReceiver<SocketAddr>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, String> {
    const PARALLEL: usize = 8;
    let mut tried: HashSet<SocketAddr> = HashSet::new();
    let mut queue: VecDeque<SocketAddr> = VecDeque::new();
    let mut attempts: JoinSet<Option<Vec<u8>>> = JoinSet::new();
    loop {
        while attempts.len() < PARALLEL {
            let Some(addr) = queue.pop_front() else { break };
            attempts.spawn(async move {
                tokio::time::timeout(
                    Duration::from_secs(30),
                    metadata_from(addr, info_hash, peer_id),
                )
                .await
                .ok()
                .and_then(Result::ok)
            });
        }
        tokio::select! {
            () = cancel.cancelled() => return Err("cancelled".into()),
            addr = peers.recv() => match addr {
                Some(addr) => {
                    if tried.insert(addr) {
                        queue.push_back(addr);
                    }
                }
                None if attempts.is_empty() && queue.is_empty() => {
                    return Err("no peer could send the torrent's details".into());
                }
                None => {
                    // No more addresses coming: just wait for the attempts.
                    if let Some(Ok(Some(info))) = attempts.join_next().await {
                        return Ok(info);
                    }
                }
            },
            Some(done) = attempts.join_next(), if !attempts.is_empty() => {
                if let Ok(Some(info)) = done {
                    return Ok(info);
                }
            }
        }
    }
}

async fn metadata_from(
    addr: SocketAddr,
    info_hash: Hash,
    peer_id: [u8; 20],
) -> Result<Vec<u8>, String> {
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| "timed out")?
        .map_err(|e| e.to_string())?;
    let ours = Handshake { info_hash, peer_id, extensions: true };
    wire::write_handshake(&mut stream, &ours).await.map_err(|e| e.to_string())?;
    let theirs = wire::read_handshake(&mut stream).await.map_err(|e| e.to_string())?;
    if theirs.info_hash != info_hash || !theirs.extensions {
        return Err("the peer can't send metadata".into());
    }
    stream.write_all(&wire::ext_handshake(None, None).encode()).await.map_err(|e| e.to_string())?;

    let mut buf = Vec::new();
    let mut parts: Vec<Option<Vec<u8>>> = Vec::new();
    let mut their_id = None;
    loop {
        buf.reserve(32 * 1024);
        if stream.read_buf(&mut buf).await.map_err(|e| e.to_string())? == 0 {
            return Err("closed".into());
        }
        while let Some(msg) = wire::take_frame(&mut buf)? {
            let Message::Extended { id, payload } = msg else { continue };
            if id == 0 {
                let ext = wire::parse_ext_handshake(&payload).ok_or("bad extension handshake")?;
                let (Some(id), Some(size)) = (ext.ut_metadata, ext.metadata_size) else {
                    return Err("the peer doesn't share metadata".into());
                };
                their_id = Some(id);
                parts = vec![None; size.div_ceil(METADATA_PIECE)];
                let mut out = Vec::new();
                for piece in 0..parts.len() as u32 {
                    out.extend(MetadataMsg::Request(piece).to_message(id).encode());
                }
                stream.write_all(&out).await.map_err(|e| e.to_string())?;
            } else if id == UT_METADATA && their_id.is_some() {
                match MetadataMsg::parse(&payload) {
                    Some(MetadataMsg::Data { piece, data, .. }) => {
                        if let Some(slot) = parts.get_mut(piece as usize) {
                            *slot = Some(data);
                        }
                    }
                    Some(MetadataMsg::Reject(_)) => return Err("the peer refused".into()),
                    _ => {}
                }
                if !parts.is_empty() && parts.iter().all(Option::is_some) {
                    let info: Vec<u8> = parts.into_iter().flatten().flatten().collect();
                    return if sha1(&info) == info_hash {
                        Ok(info)
                    } else {
                        Err("metadata failed its check".into())
                    };
                }
            }
        }
    }
}
