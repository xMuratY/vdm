//! Engine tests against vdm-testsrv: probing, multi-connection downloads with
//! dynamic splitting, resume after cuts / cancel / stale progress, remote
//! changes, retries, and the single-stream fallback.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::Value;
use vdm_core::{
    CancellationToken, DownloadRequest, Downloader, Error, Options, PART_SUFFIX, Progress,
    RetryPolicy,
};
use vdm_testsrv::{Faults, TestFile, TestServer, pattern_bytes};

const LEN: usize = 100_000;
const BIG: usize = 400_000;
const LAST_MODIFIED: &str = "Wed, 21 Oct 2015 07:28:00 GMT";

/// Fast retries, 4 connections, and segments small enough to split test-sized files.
fn downloader_with(f: impl FnOnce(&mut Options)) -> Downloader {
    let mut opts = Options {
        retry: RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(50),
        },
        connections: 4,
        min_segment: 8 * 1024,
        write_buffer: 16 * 1024,
        ..Options::default()
    };
    f(&mut opts);
    Downloader::new(opts).unwrap()
}

fn downloader() -> Downloader {
    downloader_with(|_| {})
}

async fn serve(file: TestFile, faults: Faults) -> TestServer {
    TestServer::start([("/f.bin", file)], faults).await.unwrap()
}

fn request(srv: &TestServer, dir: &Path) -> DownloadRequest {
    DownloadRequest::new(srv.url("/f.bin"), dir)
}

fn no_progress(_: Progress) {}

fn part_path(dir: &Path) -> PathBuf {
    dir.join(format!("f.bin{PART_SUFFIX}"))
}

fn sidecar_path(dir: &Path) -> PathBuf {
    dir.join(format!("f.bin{PART_SUFFIX}.json"))
}

fn read_sidecar(dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(sidecar_path(dir)).unwrap()).unwrap()
}

fn write_sidecar(dir: &Path, v: &Value) {
    std::fs::write(sidecar_path(dir), serde_json::to_vec(v).unwrap()).unwrap();
}

/// Files left in `dir`, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// `(start, end_inclusive)` of every closed `Range` request after the probe.
fn ranges(srv: &TestServer) -> Vec<(u64, u64)> {
    srv.range_headers()
        .iter()
        .flatten()
        .filter(|h| *h != "bytes=0-0")
        .map(|h| {
            let (a, b) = h.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
            (a.parse().unwrap(), b.parse().unwrap())
        })
        .collect()
}

/// 50 KB/s per connection: slow enough to cancel at a chosen point.
fn slow() -> Faults {
    Faults { throttle_bps: Some(50_000), ..Faults::default() }
}

/// Start a download and cancel it once `at` bytes have arrived.
/// Returns how many bytes were reported as kept.
async fn download_until(srv: &TestServer, dir: &Path, at: u64) -> u64 {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let err = downloader()
        .download(
            request(srv, dir),
            move |p: Progress| {
                if p.downloaded >= at {
                    trigger.cancel();
                }
            },
            cancel,
        )
        .await
        .unwrap_err();
    let Error::Cancelled { downloaded } = err else { panic!("expected cancel, got {err}") };
    assert!(downloaded >= at);
    // The part is preallocated to the full size; progress lives in the sidecar.
    assert_eq!(std::fs::metadata(part_path(dir)).unwrap().len(), LEN as u64);
    let saved: u64 = read_sidecar(dir)["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["pos"].as_u64().unwrap() - s["start"].as_u64().unwrap())
        .sum();
    assert_eq!(saved, downloaded, "sidecar matches the reported progress");
    downloaded
}

async fn download_ok(dl: &Downloader, req: DownloadRequest) -> vdm_core::Completed {
    dl.download(req, no_progress, CancellationToken::new()).await.unwrap()
}

// ---------------------------------------------------------------- probing

#[tokio::test]
async fn probe_reports_size_ranges_validators_and_name() {
    let file = TestFile::new(pattern_bytes(LEN, 1))
        .with_content_disposition(r#"attachment; filename="report final.pdf""#);
    let srv = serve(file, Faults::default()).await;
    let info =
        downloader().probe(&srv.url("/f.bin"), &[], &CancellationToken::new()).await.unwrap();
    assert_eq!(info.size, Some(LEN as u64));
    assert!(info.resumable);
    assert_eq!(info.etag.as_deref(), Some("\"len-100000\""));
    assert_eq!(info.last_modified.as_deref(), Some(LAST_MODIFIED));
    assert_eq!(info.file_name, "report final.pdf");
    assert_eq!(info.final_url, srv.url("/f.bin"));
    assert_eq!(srv.range_headers(), vec![Some("bytes=0-0".to_owned())]);
}

#[tokio::test]
async fn probe_without_range_support() {
    let faults = Faults { no_ranges: true, ..Faults::default() };
    let srv = serve(TestFile::new(pattern_bytes(LEN, 1)), faults).await;
    let info =
        downloader().probe(&srv.url("/f.bin"), &[], &CancellationToken::new()).await.unwrap();
    assert_eq!(info.size, Some(LEN as u64));
    assert!(!info.resumable);
}

#[tokio::test]
async fn probe_unknown_size() {
    let faults = Faults { no_ranges: true, hide_length: true, ..Faults::default() };
    let srv = serve(TestFile::new(pattern_bytes(LEN, 1)), faults).await;
    let info =
        downloader().probe(&srv.url("/f.bin"), &[], &CancellationToken::new()).await.unwrap();
    assert_eq!(info.size, None);
    assert!(!info.resumable);
}

#[tokio::test]
async fn probe_takes_name_from_url() {
    let srv = TestServer::start(
        [("/dir/my%20file.iso", TestFile::new(pattern_bytes(10, 1)))],
        Faults::default(),
    )
    .await
    .unwrap();
    let info = downloader()
        .probe(&srv.url("/dir/my%20file.iso"), &[], &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(info.file_name, "my file.iso");
}

// ------------------------------------------------------- multi-connection

#[tokio::test]
async fn full_download() {
    let data = pattern_bytes(LEN, 2);
    let srv = serve(TestFile::new(data.clone()), Faults::default()).await;
    let dir = tempfile::tempdir().unwrap();

    let mut last = None;
    let done = downloader()
        .download(request(&srv, dir.path()), |p: Progress| last = Some(p), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(done.path, dir.path().join("f.bin"));
    assert_eq!(done.size, LEN as u64);
    assert_eq!((done.resumed_from, done.restarted), (0, false));
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    assert_eq!(listing(dir.path()), ["f.bin"], "part and sidecar are cleaned up");
    assert_eq!(last, Some(Progress { downloaded: LEN as u64, total: Some(LEN as u64) }));
}

#[tokio::test]
async fn uses_all_connections_with_closed_ranges() {
    let data = pattern_bytes(BIG, 20);
    let faults = Faults { throttle_bps: Some(400_000), ..Faults::default() };
    let srv = serve(TestFile::new(data.clone()), faults).await;
    let dir = tempfile::tempdir().unwrap();

    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    assert_eq!(srv.max_in_flight(), 4);
    let r = ranges(&srv);
    assert!(r.contains(&(0, 99_999)) && r.contains(&(300_000, 399_999)), "{r:?}");
}

#[tokio::test]
async fn parallel_connections_are_faster() {
    let data = pattern_bytes(BIG, 21);
    // Per-connection limit: one connection needs ~1 s, four need ~0.25 s.
    let faults = Faults { throttle_bps: Some(400_000), ..Faults::default() };
    let srv = serve(TestFile::new(data), faults).await;

    let time = |connections: u8| {
        let srv = &srv;
        async move {
            let dir = tempfile::tempdir().unwrap();
            let mut req = request(srv, dir.path());
            req.connections = Some(connections);
            let started = Instant::now();
            download_ok(&downloader(), req).await;
            started.elapsed()
        }
    };
    let one = time(1).await;
    let four = time(4).await;
    assert!(four.as_secs_f64() < one.as_secs_f64() * 0.6, "1 conn {one:?}, 4 conns {four:?}");
}

#[tokio::test]
async fn idle_connections_split_the_remaining_segment() {
    let data = pattern_bytes(BIG, 22);
    let faults = Faults { throttle_bps: Some(200_000), ..Faults::default() };
    let srv = serve(TestFile::new(data.clone()), faults).await;
    let dir = tempfile::tempdir().unwrap();

    // An earlier run finished the first half; only one segment is left.
    let mut part = data[..BIG / 2].to_vec();
    part.resize(BIG, 0);
    std::fs::write(part_path(dir.path()), part).unwrap();
    write_sidecar(
        dir.path(),
        &serde_json::json!({
            "version": 2,
            "url": srv.url("/f.bin"),
            "size": BIG,
            "etag": format!("\"len-{BIG}\""),
            "last_modified": LAST_MODIFIED,
            "segments": [
                { "start": 0, "end": BIG / 2, "pos": BIG / 2 },
                { "start": BIG / 2, "end": BIG, "pos": BIG / 2 },
            ],
        }),
    );

    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!(done.resumed_from, (BIG / 2) as u64);
    assert!(!done.restarted);
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    // The other three connections split the one remaining segment.
    assert_eq!(srv.max_in_flight(), 4);
    let r = ranges(&srv);
    assert!(r.len() >= 4, "{r:?}");
    assert!(r.iter().all(|&(start, _)| start >= (BIG / 2) as u64), "re-fetched done bytes: {r:?}");
}

#[tokio::test]
async fn resumes_when_every_connection_drops() {
    let data = pattern_bytes(LEN, 3);
    let faults = Faults { cut_after: Some(10_000), ..Faults::default() };
    let srv = serve(TestFile::new(data.clone()), faults).await;
    let dir = tempfile::tempdir().unwrap();

    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    assert!(ranges(&srv).len() >= 10, "each 25 KB segment needs several requests");
}

#[tokio::test]
async fn survives_some_connections_dropping() {
    let data = pattern_bytes(BIG, 23);
    let faults = Faults { cut_after: Some(15_000), cut_every: Some(3), ..Faults::default() };
    let srv = serve(TestFile::new(data.clone()), faults).await;
    let dir = tempfile::tempdir().unwrap();

    let done =
        download_ok(&downloader_with(|o| o.connections = 8), request(&srv, dir.path())).await;
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
}

#[tokio::test]
async fn copes_with_a_server_connection_limit() {
    let data = pattern_bytes(BIG, 24);
    let faults =
        Faults { max_concurrent: Some(2), throttle_bps: Some(800_000), ..Faults::default() };
    let srv = serve(TestFile::new(data.clone()), faults).await;
    let dir = tempfile::tempdir().unwrap();

    // Six connections, but the server only allows two: the rest get 503s and
    // give up or retry, and the download still completes.
    let done =
        download_ok(&downloader_with(|o| o.connections = 6), request(&srv, dir.path())).await;
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    assert!(srv.max_in_flight() <= 2);
}

// ------------------------------------------------------------------ resume

#[tokio::test]
async fn cancel_then_resume_in_a_new_run() {
    let data = pattern_bytes(LEN, 4);
    let srv = serve(TestFile::new(data.clone()), slow()).await;
    let dir = tempfile::tempdir().unwrap();

    let kept = download_until(&srv, dir.path(), 30_000).await;
    assert!(kept < LEN as u64, "cancel should stop before the end");

    srv.set_faults(Faults::default());
    let before = ranges(&srv).len();
    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!(done.resumed_from, kept);
    assert!(!done.restarted);
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    // Every request of the second run continues a segment; none starts over.
    let starts: Vec<u64> = ranges(&srv)[before..].iter().map(|r| r.0).collect();
    assert!(!starts.contains(&0), "segment 0 was not restarted: {starts:?}");
}

#[tokio::test]
async fn stale_progress_record_only_costs_a_refetch() {
    let data = pattern_bytes(LEN, 5);
    let srv = serve(TestFile::new(data.clone()), slow()).await;
    let dir = tempfile::tempdir().unwrap();
    download_until(&srv, dir.path(), 40_000).await;

    // A crash between saves leaves a record older than the data on disk.
    let mut meta = read_sidecar(dir.path());
    let mut claimed = 0;
    for seg in meta["segments"].as_array_mut().unwrap() {
        let start = seg["start"].as_u64().unwrap();
        let pos = seg["pos"].as_u64().unwrap();
        let older = start + (pos - start) / 2;
        seg["pos"] = older.into();
        claimed += older - start;
    }
    write_sidecar(dir.path(), &meta);

    srv.set_faults(Faults::default());
    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!(done.resumed_from, claimed);
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
}

#[tokio::test]
async fn unusable_progress_record_restarts() {
    let data = pattern_bytes(LEN, 6);
    let srv = serve(TestFile::new(data.clone()), slow()).await;
    let dir = tempfile::tempdir().unwrap();
    download_until(&srv, dir.path(), 30_000).await;

    let mut meta = read_sidecar(dir.path());
    meta["segments"][0]["end"] = 1.into(); // leaves a gap
    write_sidecar(dir.path(), &meta);

    srv.set_faults(Faults::default());
    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!((done.resumed_from, done.restarted), (0, true));
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
}

#[tokio::test]
async fn part_with_wrong_size_restarts() {
    let data = pattern_bytes(LEN, 7);
    let srv = serve(TestFile::new(data.clone()), slow()).await;
    let dir = tempfile::tempdir().unwrap();
    download_until(&srv, dir.path(), 30_000).await;
    std::fs::OpenOptions::new()
        .write(true)
        .open(part_path(dir.path()))
        .unwrap()
        .set_len((LEN / 2) as u64)
        .unwrap();

    srv.set_faults(Faults::default());
    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!((done.resumed_from, done.restarted), (0, true));
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
}

#[tokio::test]
async fn file_changed_between_runs_restarts() {
    let srv = serve(TestFile::new(pattern_bytes(LEN, 8)), slow()).await;
    let dir = tempfile::tempdir().unwrap();
    download_until(&srv, dir.path(), 30_000).await;

    let new = pattern_bytes(LEN, 88);
    srv.set_file("/f.bin", TestFile::new(new.clone()).with_etag("\"v2\""));
    srv.set_faults(Faults::default());
    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!((done.resumed_from, done.restarted), (0, true));
    assert_eq!(std::fs::read(&done.path).unwrap(), new);
}

#[tokio::test]
async fn file_changed_between_runs_fails_without_restart() {
    let srv = serve(TestFile::new(pattern_bytes(LEN, 9)), slow()).await;
    let dir = tempfile::tempdir().unwrap();
    download_until(&srv, dir.path(), 30_000).await;
    let before = read_sidecar(dir.path());

    srv.set_file("/f.bin", TestFile::new(pattern_bytes(LEN, 99)).with_etag("\"v2\""));
    let err = downloader_with(|o| o.restart_if_changed = false)
        .download(request(&srv, dir.path()), no_progress, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::RemoteChanged), "got {err}");
    assert_eq!(read_sidecar(dir.path()), before, "earlier progress is left untouched");
}

#[tokio::test]
async fn file_changed_mid_download_is_caught_by_if_range() {
    let faults =
        Faults { cut_after: Some(10_000), throttle_bps: Some(100_000), ..Faults::default() };
    let srv = serve(TestFile::new(pattern_bytes(LEN, 10)), faults).await;
    let dir = tempfile::tempdir().unwrap();
    let new = pattern_bytes(LEN, 110);

    let mut changed = false;
    let done = downloader()
        .download(
            request(&srv, dir.path()),
            |p: Progress| {
                if !changed && p.downloaded > 0 {
                    srv.set_file("/f.bin", TestFile::new(new.clone()).with_etag("\"v2\""));
                    changed = true;
                }
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(changed);
    assert!(done.restarted);
    assert_eq!(std::fs::read(&done.path).unwrap(), new);
}

// ------------------------------------------------------- single stream

#[tokio::test]
async fn server_without_ranges_downloads_in_one_go() {
    let data = pattern_bytes(LEN, 11);
    let faults = Faults { no_ranges: true, ..Faults::default() };
    let srv = serve(TestFile::new(data.clone()), faults).await;
    let dir = tempfile::tempdir().unwrap();
    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    assert_eq!(srv.requests(), 2, "probe + one stream, no extra connections");
}

#[tokio::test]
async fn server_without_ranges_gives_up_when_always_cut() {
    let faults = Faults { no_ranges: true, cut_after: Some(20_000), ..Faults::default() };
    let srv = serve(TestFile::new(pattern_bytes(LEN, 12)), faults).await;
    let dir = tempfile::tempdir().unwrap();
    let err = downloader()
        .download(request(&srv, dir.path()), no_progress, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(err.is_transient(), "expected a network error, got {err}");
    // 1 probe + 5 attempts; restarts from zero never count as progress.
    assert_eq!(srv.requests(), 6);
    assert!(srv.range_headers()[1..].iter().all(Option::is_none), "never sends Range");
}

#[tokio::test]
async fn unknown_length_downloads_to_end_of_body() {
    let data = pattern_bytes(LEN, 13);
    let faults = Faults { no_ranges: true, hide_length: true, ..Faults::default() };
    let srv = serve(TestFile::new(data.clone()), faults).await;
    let dir = tempfile::tempdir().unwrap();
    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!(done.size, LEN as u64);
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
}

// ------------------------------------------------------------ misc

#[tokio::test]
async fn retries_503_then_succeeds() {
    let data = pattern_bytes(LEN, 14);
    let srv =
        serve(TestFile::new(data.clone()), Faults { fail_first: 2, ..Faults::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let done =
        download_ok(&downloader_with(|o| o.connections = 1), request(&srv, dir.path())).await;
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    assert_eq!(srv.requests(), 4, "2 failed probes, 1 probe, 1 download");
}

#[tokio::test]
async fn not_found_is_not_retried() {
    let srv = serve(TestFile::new(pattern_bytes(10, 1)), Faults::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let err = downloader()
        .download(
            DownloadRequest::new(srv.url("/missing"), dir.path()),
            no_progress,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Status { status, .. } if status == 404), "got {err}");
    assert_eq!(srv.requests(), 1);
    assert!(listing(dir.path()).is_empty());
}

#[tokio::test]
async fn existing_file_gets_numbered_name() {
    let data = pattern_bytes(LEN, 15);
    let srv = serve(TestFile::new(data.clone()), Faults::default()).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.bin"), b"keep me").unwrap();

    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!(done.path, dir.path().join("f (1).bin"));
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), b"keep me");
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
}

#[tokio::test]
async fn explicit_name_is_sanitized() {
    let srv = serve(TestFile::new(pattern_bytes(10, 16)), Faults::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&srv, dir.path());
    req.file_name = Some("../escape?.txt".into());
    let done = download_ok(&downloader(), req).await;
    assert_eq!(done.path, dir.path().join("escape_.txt"));
}

#[tokio::test]
async fn empty_file() {
    let srv = serve(TestFile::new(Bytes::new()), Faults::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let done = download_ok(&downloader(), request(&srv, dir.path())).await;
    assert_eq!(done.size, 0);
    assert_eq!(std::fs::read(&done.path).unwrap(), b"");
}

#[tokio::test]
async fn cancel_before_start() {
    let srv = serve(TestFile::new(pattern_bytes(10, 17)), Faults::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err =
        downloader().download(request(&srv, dir.path()), no_progress, cancel).await.unwrap_err();
    assert!(matches!(err, Error::Cancelled { downloaded: 0 }), "got {err}");
}

// ------------------------------------------------------------ speed limits

/// A limited download must not finish early (the limit held). It may run late:
/// shared CI machines (macOS especially) are often slower than the limit.
fn assert_took(secs: f64, expected: f64) {
    assert!(
        (expected * 0.92..expected * 1.5).contains(&secs),
        "took {secs:.3}s, expected ~{expected}s"
    );
}

/// Elapsed time of a download of `len` bytes under `limits`.
async fn timed_download(
    len: usize,
    connections: u8,
    limits: Vec<std::sync::Arc<vdm_core::RateLimiter>>,
) -> f64 {
    let data = pattern_bytes(len, 30);
    let srv = serve(TestFile::new(data.clone()), Faults::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&srv, dir.path());
    req.connections = Some(connections);
    req.limits = limits;
    let started = Instant::now();
    let done = download_ok(&downloader(), req).await;
    let secs = started.elapsed().as_secs_f64();
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    secs
}

#[tokio::test]
async fn speed_limit_is_accurate_across_connections() {
    use std::sync::Arc;
    // 600 KB at 300 KB/s over 4 connections: 2 s.
    let limit = Arc::new(vdm_core::RateLimiter::new(Some(300_000)));
    let secs = timed_download(600_000, 4, vec![limit]).await;
    assert_took(secs, 2.0);
}

#[tokio::test]
async fn slowest_of_several_limits_wins() {
    use std::sync::Arc;
    let global = Arc::new(vdm_core::RateLimiter::new(Some(10_000_000)));
    let own = Arc::new(vdm_core::RateLimiter::new(Some(200_000)));
    let secs = timed_download(300_000, 2, vec![global, own]).await;
    assert_took(secs, 1.5);
}

#[tokio::test]
async fn speed_limit_applies_to_single_stream_servers() {
    use std::sync::Arc;
    let data = pattern_bytes(200_000, 31);
    let faults = Faults { no_ranges: true, ..Faults::default() };
    let srv = serve(TestFile::new(data.clone()), faults).await;
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&srv, dir.path());
    req.limits = vec![Arc::new(vdm_core::RateLimiter::new(Some(200_000)))];
    let started = Instant::now();
    download_ok(&downloader(), req).await;
    let secs = started.elapsed().as_secs_f64();
    assert_took(secs, 1.0);
}

#[tokio::test]
async fn same_url_twice_at_once_gets_two_files() {
    let data = pattern_bytes(LEN, 40);
    let srv = serve(TestFile::new(data.clone()), slow()).await;
    let dir = tempfile::tempdir().unwrap();
    let dl = downloader();
    let (a, b) = tokio::join!(
        download_ok(&dl, request(&srv, dir.path())),
        download_ok(&dl, request(&srv, dir.path())),
    );
    let mut names = [a.path.clone(), b.path.clone()].map(|p| p.file_name().unwrap().to_owned());
    names.sort();
    assert_eq!(names, ["f (1).bin", "f.bin"].map(std::ffi::OsString::from));
    assert_eq!(std::fs::read(&a.path).unwrap(), data);
    assert_eq!(std::fs::read(&b.path).unwrap(), data);
}

// ------------------------------------------------------- logins and proxy

#[tokio::test]
async fn site_login_is_sent_to_its_site_only() {
    let srv = serve(TestFile::new(pattern_bytes(LEN, 40)), Faults::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let login = |site: &str| vdm_core::SiteLogin {
        site: site.into(),
        username: "ann".into(),
        password: "s3cret".into(),
    };
    let dl = downloader_with(|o| o.logins = vec![login("127.0.0.1")]);
    download_ok(&dl, request(&srv, dir.path())).await;
    let auth = srv.header_values("authorization");
    assert!(auth.len() >= 2);
    assert!(auth.iter().all(|a| a.as_deref() == Some("Basic YW5uOnMzY3JldA==")), "{auth:?}");

    let other = serve(TestFile::new(pattern_bytes(LEN, 41)), Faults::default()).await;
    let dl = downloader_with(|o| o.logins = vec![login("files.example.com")]);
    download_ok(&dl, request(&other, tempfile::tempdir().unwrap().path())).await;
    assert!(other.header_values("authorization").iter().all(Option::is_none));
}

#[tokio::test]
async fn downloads_go_through_the_configured_proxy() {
    // The test server acts as the HTTP proxy for a host that doesn't exist.
    let data = pattern_bytes(LEN, 42);
    let proxy = serve(TestFile::new(data.clone()), Faults::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let dl = downloader_with(|o| {
        o.proxy = vdm_core::ProxySetting::Url(format!("http://{}", proxy.addr()));
    });
    let req = DownloadRequest::new("http://downloads.example.invalid/f.bin", dir.path());
    let done = download_ok(&dl, req).await;
    assert_eq!(std::fs::read(&done.path).unwrap(), data);
    assert!(proxy.requests() >= 2, "probe and download went through the proxy");
}
