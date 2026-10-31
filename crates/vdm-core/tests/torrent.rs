//! Torrents end to end on localhost: a tiny HTTP tracker, our own seeder, and
//! a downloader fetching from it by `.torrent` file and by magnet link. A spy
//! peer checks that download-only mode never uploads.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use vdm_core::torrent::make_torrent;
use vdm_core::{CancellationToken, DownloadRequest, Downloader, Options, Progress, Seed};

/// Announced peers by peer id: (address, port they accept connections on).
type Peers = Arc<Mutex<HashMap<Vec<u8>, SocketAddr>>>;

/// An HTTP tracker that remembers who announced and hands out everyone else.
async fn tracker() -> (String, Peers) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/announce", listener.local_addr().unwrap());
    let peers: Peers = Arc::default();
    let table = peers.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, from)) = listener.accept().await else { return };
            let table = table.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let mut len = 0;
                while !buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = stream.read(&mut buf[len..]).await else { return };
                    if n == 0 {
                        return;
                    }
                    len += n;
                }
                let request = String::from_utf8_lossy(&buf[..len]).into_owned();
                let query = request.split(['?', ' ']).nth(2).unwrap_or_default().to_owned();
                let param = |name: &str| {
                    query
                        .split('&')
                        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
                        .unwrap_or_default()
                        .to_owned()
                };
                let peer_id = percent_decode(&param("peer_id"));
                let port: u16 = param("port").parse().unwrap_or(0);
                let mut compact = Vec::new();
                {
                    let mut t = table.lock().unwrap();
                    if port != 0 {
                        t.insert(peer_id.clone(), SocketAddr::new(from.ip(), port));
                    }
                    for (id, addr) in t.iter() {
                        if *id != peer_id
                            && let SocketAddr::V4(v4) = addr
                        {
                            compact.extend_from_slice(&v4.ip().octets());
                            compact.extend_from_slice(&v4.port().to_be_bytes());
                        }
                    }
                }
                let mut body = format!("d8:intervali60e5:peers{}:", compact.len()).into_bytes();
                body.extend_from_slice(&compact);
                body.push(b'e');
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    });
    (url, peers)
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let hex = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(v) if b[i] == b'%' => {
                out.push(v);
                i += 3;
            }
            _ => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    out
}

fn options(upload: bool) -> Options {
    // Test seeders keep going however much they upload.
    Options { torrent_upload: upload, torrent_seed_ratio: None, ..Options::default() }
}

fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[1] ^ seed).collect()
}

/// Seed `files` from `dir` until cancelled; returns the `.torrent` bytes.
async fn start_seeder(
    dir: &Path,
    name: &str,
    files: &[(&str, &[u8])],
    tracker: &str,
    cancel: CancellationToken,
) -> Vec<u8> {
    let single = files.len() == 1 && files[0].0 == name;
    let content_path = dir.join(name);
    for (path, data) in files {
        let file = if single { content_path.clone() } else { content_path.join(path) };
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, data).unwrap();
    }
    let torrent = make_torrent(name, files, 32 * 1024, tracker);
    let seed = Seed::new(torrent.clone(), content_path);
    tokio::spawn(async move {
        let dl = Downloader::new(options(true)).unwrap();
        let _ = dl.seed(&seed, &[], &cancel).await;
    });
    torrent
}

async fn wait_for_peers(peers: &Peers, n: usize) {
    for _ in 0..100 {
        if peers.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("peers never announced");
}

fn file_url(path: &Path) -> String {
    reqwest::Url::from_file_path(path).unwrap().to_string()
}

#[tokio::test]
async fn downloads_a_folder_from_our_own_seeder() {
    let (tracker_url, peers) = tracker().await;
    let seed_dir = tempfile::tempdir().unwrap();
    let a = content(300_000, 1);
    let b = content(70_001, 2);
    let stop = CancellationToken::new();
    let torrent = start_seeder(
        seed_dir.path(),
        "Album",
        &[("cd1/a.bin", &a), ("b.bin", &b)],
        &tracker_url,
        stop.clone(),
    )
    .await;
    wait_for_peers(&peers, 1).await;

    let dir = tempfile::tempdir().unwrap();
    let torrent_path = dir.path().join("album.torrent");
    std::fs::write(&torrent_path, &torrent).unwrap();
    let out = dir.path().join("out");
    let dl = Downloader::new(options(false)).unwrap();
    let mut last = None;
    let done = tokio::time::timeout(
        Duration::from_secs(30),
        dl.download(
            DownloadRequest::new(file_url(&torrent_path), &out),
            |p: Progress| last = Some(p),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("finished in time")
    .unwrap();
    stop.cancel();

    assert_eq!(done.path, out.join("Album"));
    assert_eq!(std::fs::read(done.path.join("cd1").join("a.bin")).unwrap(), a);
    assert_eq!(std::fs::read(done.path.join("b.bin")).unwrap(), b);
    assert_eq!(done.size, (a.len() + b.len()) as u64);
    assert!(done.seed.is_none(), "download only: nothing to seed");
    assert_eq!(last.map(|p| p.downloaded), Some(done.size));
    // Only the finished folder is left: no work folder or sidecar.
    let left: Vec<_> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, ["Album"]);
}

#[tokio::test]
async fn a_magnet_link_fetches_the_details_from_peers() {
    let (tracker_url, peers) = tracker().await;
    let seed_dir = tempfile::tempdir().unwrap();
    let data = content(100_000, 3);
    let stop = CancellationToken::new();
    let torrent = start_seeder(
        seed_dir.path(),
        "movie.mkv",
        &[("movie.mkv", &data)],
        &tracker_url,
        stop.clone(),
    )
    .await;
    wait_for_peers(&peers, 1).await;

    let hash = info_hash_hex(&torrent);
    let magnet = format!(
        "magnet:?xt=urn:btih:{hash}&tr={}",
        tracker_url.replace(':', "%3A").replace('/', "%2F")
    );
    let dir = tempfile::tempdir().unwrap();
    let dl = Downloader::new(options(true)).unwrap();
    let probe = dl.probe(&magnet, &[], &CancellationToken::new()).await.unwrap();
    assert_eq!(probe.file_name, hash, "no dn: the hash stands in for the name");

    let done = tokio::time::timeout(
        Duration::from_secs(30),
        dl.download(
            DownloadRequest::new(&magnet, dir.path()),
            |_: Progress| {},
            CancellationToken::new(),
        ),
    )
    .await
    .expect("finished in time")
    .unwrap();
    stop.cancel();
    assert_eq!(done.path, dir.path().join("movie.mkv"));
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    let seed = done.seed.expect("uploading is on, so it can seed");
    assert_eq!(seed.content, done.path);
}

/// The info hash of a `.torrent` we built: SHA-1 of its info dictionary.
fn info_hash_hex(torrent: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let start = torrent.windows(6).position(|w| w == b"4:info").unwrap() + 6;
    let info = &torrent[start..torrent.len() - 1];
    Sha1::digest(info).iter().map(|b| format!("{b:02x}")).collect()
}

/// A peer that only asks for data, and notes the message ids each
/// connection sends it.
async fn spy(info_hash: [u8; 20], tracker: &str) -> Arc<Mutex<Vec<Vec<u8>>>> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let got = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let got = got.clone();
            let conn = {
                let mut all = got.lock().unwrap();
                all.push(Vec::new());
                all.len() - 1
            };
            tokio::spawn(async move {
                let mut hs = [0u8; 68];
                if s.read_exact(&mut hs).await.is_err() {
                    return;
                }
                let mut ours = vec![19u8];
                ours.extend_from_slice(b"BitTorrent protocol");
                ours.extend_from_slice(&[0; 8]);
                ours.extend_from_slice(&info_hash);
                ours.extend_from_slice(b"-SP0001-spyspyspyspy");
                // Interested, then a request for the first block.
                ours.extend_from_slice(&[0, 0, 0, 1, 2]);
                ours.extend_from_slice(&[0, 0, 0, 13, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0]);
                let _ = s.write_all(&ours).await;
                let mut buf = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    let Ok(n) = s.read(&mut chunk).await else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    while buf.len() >= 4 {
                        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                        if buf.len() < 4 + len {
                            break;
                        }
                        if len > 0 {
                            got.lock().unwrap()[conn].push(buf[4]);
                        }
                        buf.drain(..4 + len);
                    }
                }
            });
        }
    });
    // Register with the tracker like any peer.
    let peer_id = "-SP0001-spyspyspyspy";
    let hash: String = info_hash.iter().map(|b| format!("%{b:02X}")).collect();
    let url = format!("{tracker}?info_hash={hash}&peer_id={peer_id}&port={port}&left=0&compact=1");
    reqwest::get(url).await.unwrap();
    seen
}

#[tokio::test]
async fn download_only_never_uploads() {
    let (tracker_url, peers) = tracker().await;
    let seed_dir = tempfile::tempdir().unwrap();
    let data = content(3_000_000, 4);
    let stop = CancellationToken::new();
    let torrent =
        start_seeder(seed_dir.path(), "x.bin", &[("x.bin", &data)], &tracker_url, stop.clone())
            .await;
    let hash_hex = info_hash_hex(&torrent);
    let info_hash: [u8; 20] = (0..20)
        .map(|i| u8::from_str_radix(&hash_hex[i * 2..i * 2 + 2], 16).unwrap())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let seen = spy(info_hash, &tracker_url).await;
    wait_for_peers(&peers, 2).await;

    let dir = tempfile::tempdir().unwrap();
    let torrent_path = dir.path().join("x.torrent");
    std::fs::write(&torrent_path, &torrent).unwrap();
    let dl = Downloader::new(options(false)).unwrap();
    let done = tokio::time::timeout(
        Duration::from_secs(30),
        dl.download(
            DownloadRequest::new(file_url(&torrent_path), dir.path()),
            |_: Progress| {},
            CancellationToken::new(),
        ),
    )
    .await
    .expect("finished in time")
    .unwrap();
    stop.cancel();
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    let seen = seen.lock().unwrap().clone();
    // The downloader is the connection that announces new pieces (4 = have);
    // the seeder has them all from the start.
    let ours = seen.iter().find(|ids| ids.contains(&4)).expect("the downloader talked to the spy");
    // 1 = unchoke, 7 = piece: neither may reach a peer in download-only mode.
    assert!(!ours.contains(&1) && !ours.contains(&7), "messages seen: {ours:?}");
}

#[tokio::test]
async fn a_paused_torrent_resumes_where_it_stopped() {
    let (tracker_url, peers) = tracker().await;
    let seed_dir = tempfile::tempdir().unwrap();
    let data = content(1_000_000, 5);
    let stop = CancellationToken::new();
    let torrent =
        start_seeder(seed_dir.path(), "big.bin", &[("big.bin", &data)], &tracker_url, stop.clone())
            .await;
    wait_for_peers(&peers, 1).await;

    let dir = tempfile::tempdir().unwrap();
    let torrent_path = dir.path().join("big.torrent");
    std::fs::write(&torrent_path, &torrent).unwrap();
    let dl = Downloader::new(options(false)).unwrap();
    // Slowly, so it can be stopped partway.
    let slow = Arc::new(vdm_core::RateLimiter::new(Some(400_000)));
    let mut req = DownloadRequest::new(file_url(&torrent_path), dir.path());
    req.limits = vec![slow];
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let err = dl
        .download(
            req.clone(),
            move |p: Progress| {
                if p.downloaded >= 300_000 {
                    trigger.cancel();
                }
            },
            cancel,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, vdm_core::Error::Cancelled { .. }), "{err}");
    // The work folder and its sidecar stay for the resume.
    assert!(dir.path().join("big.bin.vdmpart").join("resume.bin").exists());
    // The torrent file may go away: the work folder kept a copy.
    std::fs::remove_file(&torrent_path).unwrap();

    req.limits.clear();
    req.file_name = Some("big.bin".into());
    let done = tokio::time::timeout(
        Duration::from_secs(30),
        dl.download(req, |_: Progress| {}, CancellationToken::new()),
    )
    .await
    .expect("finished in time")
    .unwrap();
    stop.cancel();
    assert!(done.resumed_from >= 200_000, "resumed from {}", done.resumed_from);
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    assert!(!dir.path().join("big.bin.vdmpart").exists());
}
