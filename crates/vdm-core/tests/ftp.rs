//! FTP through the real downloader and engine, against an in-process
//! libunftp server: probe, full download, cancel + resume, and the engine.

use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use libunftp::ServerBuilder;
use unftp_sbe_fs::Filesystem;
use vdm_core::{
    CancellationToken, DownloadRequest, Downloader, Error, Options, Progress, RateLimiter,
    RetryPolicy,
};

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 13) as u8 ^ seed).collect()
}

/// An anonymous FTP server over `root` on a free port.
async fn serve(root: &Path) -> u16 {
    let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.local_addr())
        .unwrap()
        .port();
    let root = root.to_owned();
    let server = ServerBuilder::new(Box::new(move || Filesystem::new(root.clone()).unwrap()))
        .build()
        .unwrap();
    tokio::spawn(async move {
        let _ = server.listen(format!("127.0.0.1:{port}")).await;
    });
    for _ in 0..500 {
        if tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.is_ok() {
            return port;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("FTP test server did not start");
}

fn downloader() -> Downloader {
    Downloader::new(Options {
        retry: RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        },
        write_buffer: 16 * 1024,
        ..Options::default()
    })
    .unwrap()
}

#[tokio::test]
async fn probe_and_full_download() {
    let root = tempfile::tempdir().unwrap();
    let data = pattern(700_000, 1);
    std::fs::write(root.path().join("release notes.bin"), &data).unwrap();
    let port = serve(root.path()).await;
    let url = format!("ftp://127.0.0.1:{port}/release%20notes.bin");

    let info = downloader().probe(&url, &[], &CancellationToken::new()).await.unwrap();
    assert_eq!(info.size, Some(data.len() as u64));
    assert!(info.resumable);
    assert_eq!(info.file_name, "release notes.bin");

    let dir = tempfile::tempdir().unwrap();
    let done = downloader()
        .download(DownloadRequest::new(url, dir.path()), |_: Progress| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(done.path, dir.path().join("release notes.bin"));
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
}

#[tokio::test]
async fn cancel_then_resume_with_rest() {
    let root = tempfile::tempdir().unwrap();
    let data = pattern(1_000_000, 2);
    std::fs::write(root.path().join("big.bin"), &data).unwrap();
    let port = serve(root.path()).await;
    let url = format!("ftp://127.0.0.1:{port}/big.bin");
    let dir = tempfile::tempdir().unwrap();

    // Slow it down with a speed limit, and stop part-way.
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let mut req = DownloadRequest::new(url.clone(), dir.path());
    req.limits = vec![Arc::new(RateLimiter::new(Some(500_000)))];
    let err = downloader()
        .download(
            req,
            move |p: Progress| {
                if p.downloaded >= 300_000 {
                    trigger.cancel();
                }
            },
            cancel,
        )
        .await
        .unwrap_err();
    let Error::Cancelled { downloaded } = err else { panic!("expected cancel, got {err}") };
    assert!((300_000..1_000_000).contains(&downloaded), "{downloaded}");

    let done = downloader()
        .download(DownloadRequest::new(url, dir.path()), |_: Progress| {}, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(done.resumed_from, downloaded);
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
}

#[tokio::test]
async fn missing_file_fails_without_retrying_forever() {
    let root = tempfile::tempdir().unwrap();
    let port = serve(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let err = downloader()
        .download(
            DownloadRequest::new(format!("ftp://127.0.0.1:{port}/nope.bin"), dir.path()),
            |_: Progress| {},
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Ftp { transient: false, .. }), "{err}");
}

#[tokio::test]
async fn engine_downloads_ftp_urls() {
    use vdm_core::{Engine, EngineConfig};
    use vdm_proto::{Event, NewDownload};

    let root = tempfile::tempdir().unwrap();
    let data = pattern(200_000, 3);
    std::fs::write(root.path().join("tool.zip"), &data).unwrap();
    let port = serve(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let engine =
        Engine::start(EngineConfig::new(dir.path()), vdm_store::Store::open_in_memory().unwrap())
            .unwrap();
    let mut rx = engine.subscribe();
    let id = engine
        .add(NewDownload {
            url: format!("ftp://127.0.0.1:{port}/tool.zip"),
            save_dir: None,
            file_name: None,
            connections: None,
            referrer: None,
            user_agent: None,
            cookies: None,
            checksum: None,
            queue: None,
        })
        .await
        .unwrap();
    let path = loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap() {
            Event::Completed { id: i, path } if i == id => break path,
            Event::Failed { id: i, error } if i == id => panic!("failed: {error}"),
            _ => {}
        }
    };
    assert_eq!(std::fs::read(path).unwrap(), data);
}
