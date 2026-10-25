//! Engine tests against vdm-testsrv: queueing, pause/resume/remove, failures,
//! speed limits, the schedule window, and recovery across restarts.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{NaiveDate, NaiveTime};
use tokio::sync::broadcast;
use vdm_core::{Engine, EngineConfig, EngineError, Error, Options, RetryPolicy, Schedule};
use vdm_proto::{Command, DownloadId, DownloadState, Event, NewDownload, QueueId};
use vdm_store::Store;
use vdm_testsrv::{Faults, TestFile, TestServer, pattern_bytes};

const LEN: usize = 100_000;

fn config(dir: &Path) -> EngineConfig {
    let mut cfg = EngineConfig::new(dir);
    cfg.options = Options {
        retry: RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        },
        connections: 4,
        min_segment: 8 * 1024,
        write_buffer: 16 * 1024,
        ..Options::default()
    };
    cfg.schedule_check = Duration::from_millis(20);
    cfg
}

fn engine(cfg: EngineConfig) -> Engine {
    Engine::start(cfg, Store::open_in_memory().unwrap()).unwrap()
}

fn new_download(url: String) -> NewDownload {
    NewDownload {
        url,
        save_dir: None,
        file_name: None,
        connections: None,
        referrer: None,
        user_agent: None,
        cookies: None,
        checksum: None,
        queue: None,
    }
}

/// Wait (up to 10 s) for the first event matching `pred`.
async fn wait_for(
    rx: &mut broadcast::Receiver<Event>,
    mut pred: impl FnMut(&Event) -> bool,
) -> Event {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(ev)) if pred(&ev) => return ev,
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) => panic!("event stream closed"),
            Err(_) => panic!("timed out waiting for an event"),
        }
    }
}

/// Wait until every id in `ids` has completed, in any order.
async fn wait_all_completed(rx: &mut broadcast::Receiver<Event>, mut ids: Vec<DownloadId>) {
    wait_for(rx, |ev| {
        if let Event::Completed { id, .. } = ev {
            ids.retain(|x| x != id);
        }
        ids.is_empty()
    })
    .await;
}

fn completed(id: DownloadId) -> impl FnMut(&Event) -> bool {
    move |ev| matches!(ev, Event::Completed { id: i, .. } if *i == id)
}

fn state(id: DownloadId, want: DownloadState) -> impl FnMut(&Event) -> bool {
    move |ev| matches!(ev, Event::StateChanged { id: i, state } if *i == id && *state == want)
}

fn progressed(id: DownloadId, at_least: u64) -> impl FnMut(&Event) -> bool {
    move |ev| matches!(ev, Event::Progress { id: i, downloaded, .. } if *i == id && *downloaded >= at_least)
}

async fn record(engine: &Engine, id: DownloadId) -> vdm_store::DownloadRecord {
    engine.list().await.unwrap().into_iter().find(|r| r.id == id).expect("record exists")
}

fn slow() -> Faults {
    Faults { throttle_bps: Some(50_000), ..Faults::default() }
}

/// Noon on a Saturday, and a window that is closed then (23:00-07:00).
fn noon() -> vdm_core::engine::Clock {
    let at = NaiveDate::from_ymd_opt(2026, 10, 24).unwrap().and_hms_opt(12, 0, 0).unwrap();
    Arc::new(move || at)
}

fn night_only() -> Schedule {
    Schedule {
        start: NaiveTime::from_hms_opt(23, 0, 0),
        stop: NaiveTime::from_hms_opt(7, 0, 0),
        days: Vec::new(),
    }
}

#[tokio::test]
async fn add_runs_to_completion() {
    let data = pattern_bytes(LEN, 1);
    let srv = TestServer::start([("/f.bin", TestFile::new(data.clone()))], Faults::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let mut rx = engine.subscribe();

    let id = engine.add(new_download(srv.url("/f.bin"))).await.unwrap();
    let mut seen = Vec::new();
    wait_for(&mut rx, |ev| {
        seen.push(ev.clone());
        completed(id)(ev)
    })
    .await;

    assert!(
        matches!(&seen[0], Event::Added { id: i, file_name, .. } if *i == id && file_name == "f.bin")
    );
    assert!(
        seen.iter().any(|e| matches!(e, Event::Started { connections: 4, resumable: true, .. }))
    );
    assert!(seen.iter().any(|e| matches!(e, Event::Progress { .. })));
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), data);
    let rec = record(&engine, id).await;
    assert_eq!(rec.state, DownloadState::Completed);
    assert_eq!((rec.file_name.as_str(), rec.size), ("f.bin", Some(LEN as u64)));
}

#[tokio::test]
async fn progress_carries_the_piece_map() {
    let srv = TestServer::start([("/f.bin", TestFile::new(pattern_bytes(LEN, 1)))], slow())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let mut rx = engine.subscribe();

    let id = engine.add(new_download(srv.url("/f.bin"))).await.unwrap();
    let ev = wait_for(&mut rx, |ev| {
        matches!(ev, Event::Progress { id: i, segments, connections, .. }
            if *i == id && !segments.is_empty() && *connections > 0)
    })
    .await;
    let Event::Progress { segments, connections, downloaded, .. } = ev else { unreachable!() };
    assert!(connections <= 4, "{connections}");
    // The pieces cover the file in order, and their progress adds up.
    assert_eq!(segments.first().map(|s| s.start), Some(0));
    assert_eq!(segments.last().map(|s| s.end), Some(LEN as u64));
    assert!(segments.windows(2).all(|w| w[0].end == w[1].start));
    let done: u64 = segments.iter().map(|s| s.pos - s.start).sum();
    assert!(done >= downloaded, "{done} < {downloaded}");
    engine.command(Command::Pause { id }).await.unwrap();
}

#[tokio::test]
async fn max_active_runs_downloads_one_at_a_time_in_order() {
    let files = ["/a.bin", "/b.bin", "/c.bin"].map(|p| (p, TestFile::new(pattern_bytes(LEN, 2))));
    let faults = Faults { throttle_bps: Some(400_000), ..Faults::default() };
    let srv = TestServer::start(files, faults).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.max_active = 1;
    let engine = engine(cfg);
    let mut rx = engine.subscribe();

    let mut ids = Vec::new();
    for p in ["/a.bin", "/b.bin", "/c.bin"] {
        ids.push(engine.add(new_download(srv.url(p))).await.unwrap());
    }
    let (mut running, mut max_running, mut order) = (0i32, 0, Vec::new());
    wait_for(&mut rx, |ev| {
        match ev {
            Event::StateChanged { state: DownloadState::Downloading, .. } => running += 1,
            Event::StateChanged { state: DownloadState::Completed, id } => {
                running -= 1;
                order.push(*id);
            }
            _ => {}
        }
        max_running = max_running.max(running);
        order.len() == 3
    })
    .await;
    assert_eq!(max_running, 1);
    assert_eq!(order, ids, "oldest first");
}

#[tokio::test]
async fn pause_and_resume() {
    let data = pattern_bytes(LEN, 3);
    let srv = TestServer::start([("/f.bin", TestFile::new(data.clone()))], slow()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let mut rx = engine.subscribe();

    let id = engine.add(new_download(srv.url("/f.bin"))).await.unwrap();
    wait_for(&mut rx, progressed(id, 20_000)).await;
    engine.command(Command::Pause { id }).await.unwrap();
    wait_for(&mut rx, state(id, DownloadState::Paused)).await;
    assert_eq!(record(&engine, id).await.state, DownloadState::Paused);

    srv.set_faults(Faults::default());
    engine.command(Command::Resume { id }).await.unwrap();
    let started = wait_for(&mut rx, |ev| matches!(ev, Event::Started { .. })).await;
    let Event::Started { resumed_from, .. } = started else { unreachable!() };
    assert!(resumed_from >= 20_000, "resumed from {resumed_from}");
    wait_for(&mut rx, completed(id)).await;
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), data);
}

#[tokio::test]
async fn pausing_a_queued_download_skips_it() {
    let files = ["/a.bin", "/b.bin"].map(|p| (p, TestFile::new(pattern_bytes(LEN, 4))));
    let srv = TestServer::start(files, slow()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.max_active = 1;
    let engine = engine(cfg);
    let mut rx = engine.subscribe();

    let a = engine.add(new_download(srv.url("/a.bin"))).await.unwrap();
    let b = engine.add(new_download(srv.url("/b.bin"))).await.unwrap();
    engine.command(Command::Pause { id: b }).await.unwrap();
    srv.set_faults(Faults::default());
    wait_for(&mut rx, completed(a)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(record(&engine, b).await.state, DownloadState::Paused);

    engine.command(Command::Resume { id: b }).await.unwrap();
    wait_for(&mut rx, completed(b)).await;
}

#[tokio::test]
async fn removing_an_active_download_deletes_its_files() {
    let srv = TestServer::start([("/f.bin", TestFile::new(pattern_bytes(LEN, 5)))], slow())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let mut rx = engine.subscribe();

    let id = engine.add(new_download(srv.url("/f.bin"))).await.unwrap();
    wait_for(&mut rx, progressed(id, 10_000)).await;
    engine.command(Command::Remove { id, delete_file: true }).await.unwrap();
    wait_for(&mut rx, |ev| matches!(ev, Event::Removed { id: i } if *i == id)).await;

    assert!(engine.list().await.unwrap().is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "part and sidecar deleted");
}

#[tokio::test]
async fn removing_a_finished_download_keeps_the_file_unless_asked() {
    let files = ["/a.bin", "/b.bin"].map(|p| (p, TestFile::new(pattern_bytes(10_000, 6))));
    let srv = TestServer::start(files, Faults::default()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let mut rx = engine.subscribe();

    let a = engine.add(new_download(srv.url("/a.bin"))).await.unwrap();
    let b = engine.add(new_download(srv.url("/b.bin"))).await.unwrap();
    wait_all_completed(&mut rx, vec![a, b]).await;

    engine.command(Command::Remove { id: a, delete_file: false }).await.unwrap();
    engine.command(Command::Remove { id: b, delete_file: true }).await.unwrap();
    assert!(dir.path().join("a.bin").exists());
    assert!(!dir.path().join("b.bin").exists());
    assert!(engine.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn failures_are_reported_and_can_be_retried() {
    let srv =
        TestServer::start([("/f.bin", TestFile::new(pattern_bytes(10, 7)))], Faults::default())
            .await
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let mut rx = engine.subscribe();

    let id = engine.add(new_download(srv.url("/missing"))).await.unwrap();
    let ev = wait_for(&mut rx, |ev| matches!(ev, Event::Failed { .. })).await;
    let Event::Failed { error, .. } = ev else { unreachable!() };
    assert!(error.contains("404"), "{error}");
    let rec = record(&engine, id).await;
    assert_eq!(rec.state, DownloadState::Failed);
    assert!(rec.error.unwrap().contains("404"));

    engine.command(Command::Resume { id }).await.unwrap();
    wait_for(&mut rx, state(id, DownloadState::Downloading)).await;
    wait_for(&mut rx, state(id, DownloadState::Failed)).await;
}

#[tokio::test]
async fn bad_requests_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let err = engine.add(new_download("file:///etc/passwd".into())).await.unwrap_err();
    assert!(matches!(err, EngineError::Download(Error::InvalidUrl(_))), "{err}");
    let err = engine.command(Command::Pause { id: DownloadId(42) }).await.unwrap_err();
    assert!(matches!(err, EngineError::NotFound(DownloadId(42))), "{err}");
    let err = engine
        .command(Command::RefreshUrl { id: DownloadId(1), url: "https://x/".into() })
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Unsupported(_)), "{err}");
}

#[tokio::test]
async fn global_speed_limit_is_shared_by_all_downloads() {
    let files = ["/a.bin", "/b.bin"].map(|p| (p, TestFile::new(pattern_bytes(300_000, 8))));
    let srv = TestServer::start(files, Faults::default()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.speed_limit = Some(300_000);
    let engine = engine(cfg);
    let mut rx = engine.subscribe();

    let started = Instant::now();
    let a = engine.add(new_download(srv.url("/a.bin"))).await.unwrap();
    let b = engine.add(new_download(srv.url("/b.bin"))).await.unwrap();
    wait_all_completed(&mut rx, vec![a, b]).await;
    // 600 KB at 300 KB/s.
    let secs = started.elapsed().as_secs_f64();
    // Never early (the limit held); slow CI machines may run late.
    assert!((1.9..3.0).contains(&secs), "took {secs:.3}s, expected ~2s");
}

#[tokio::test]
async fn per_download_speed_limit() {
    let srv = TestServer::start(
        [("/f.bin", TestFile::new(pattern_bytes(300_000, 9)))],
        Faults::default(),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    // Closed window: set the limit before anything starts.
    cfg.clock = noon();
    cfg.schedule = night_only();
    let engine = engine(cfg);
    let mut rx = engine.subscribe();

    let id = engine.add(new_download(srv.url("/f.bin"))).await.unwrap();
    engine
        .command(Command::SetSpeedLimit { id: Some(id), bytes_per_sec: Some(200_000) })
        .await
        .unwrap();
    assert_eq!(record(&engine, id).await.speed_limit, Some(200_000));

    let started = Instant::now();
    engine.set_schedule(Schedule::always()).await.unwrap();
    wait_for(&mut rx, completed(id)).await;
    let secs = started.elapsed().as_secs_f64();
    // Never early (the limit held); slow CI machines may run late.
    assert!((1.42..2.25).contains(&secs), "took {secs:.3}s, expected ~1.5s");
}

#[tokio::test]
async fn closed_schedule_window_keeps_downloads_queued() {
    let srv =
        TestServer::start([("/f.bin", TestFile::new(pattern_bytes(LEN, 10)))], Faults::default())
            .await
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.clock = noon();
    cfg.schedule = night_only();
    let engine = engine(cfg);
    let mut rx = engine.subscribe();

    let id = engine.add(new_download(srv.url("/f.bin"))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(record(&engine, id).await.state, DownloadState::Queued);
    assert_eq!(srv.requests(), 0);

    engine.set_schedule(Schedule::always()).await.unwrap();
    wait_for(&mut rx, completed(id)).await;
}

#[tokio::test]
async fn closing_window_requeues_active_downloads() {
    let data = pattern_bytes(LEN, 11);
    let srv = TestServer::start([("/f.bin", TestFile::new(data.clone()))], slow()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.clock = noon();
    let engine = engine(cfg);
    let mut rx = engine.subscribe();

    let id = engine.add(new_download(srv.url("/f.bin"))).await.unwrap();
    wait_for(&mut rx, progressed(id, 20_000)).await;
    engine.set_schedule(night_only()).await.unwrap();
    wait_for(&mut rx, state(id, DownloadState::Queued)).await;

    srv.set_faults(Faults::default());
    engine.set_schedule(Schedule::always()).await.unwrap();
    let ev = wait_for(&mut rx, |ev| matches!(ev, Event::Started { .. })).await;
    assert!(matches!(ev, Event::Started { resumed_from, .. } if resumed_from >= 20_000));
    wait_for(&mut rx, completed(id)).await;
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), data);
}

#[tokio::test]
async fn shutdown_and_restart_resumes_unfinished_downloads() {
    let data = pattern_bytes(LEN, 12);
    let srv = TestServer::start([("/f.bin", TestFile::new(data.clone()))], slow()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("vdm.db");

    let first = Engine::start(config(dir.path()), Store::open(&db_path).unwrap()).unwrap();
    let mut rx = first.subscribe();
    let id = first.add(new_download(srv.url("/f.bin"))).await.unwrap();
    wait_for(&mut rx, progressed(id, 20_000)).await;
    first.shutdown().await.unwrap();
    let rec = Store::open(&db_path).unwrap().get(id).unwrap().unwrap();
    assert_eq!(rec.state, DownloadState::Queued, "shutdown keeps it queued");

    srv.set_faults(Faults::default());
    let second = Engine::start(config(dir.path()), Store::open(&db_path).unwrap()).unwrap();
    let mut rx = second.subscribe();
    let ev = wait_for(&mut rx, |ev| matches!(ev, Event::Started { .. })).await;
    assert!(matches!(ev, Event::Started { resumed_from, .. } if resumed_from >= 20_000));
    wait_for(&mut rx, completed(id)).await;
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), data);
}

#[tokio::test]
async fn downloads_interrupted_by_a_crash_are_queued_again() {
    let srv =
        TestServer::start([("/f.bin", TestFile::new(pattern_bytes(LEN, 13)))], Faults::default())
            .await
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let id = store
        .insert_download(&vdm_store::NewRecord {
            url: srv.url("/f.bin"),
            save_dir: dir.path().to_string_lossy().into_owned(),
            file_name: String::new(),
            size: None,
            etag: None,
            last_modified: None,
            connections: 2,
            headers: Vec::new(),
            speed_limit: None,
            checksum: None,
            queue: QueueId::MAIN,
        })
        .unwrap();
    store.update_state(id, DownloadState::Downloading, None).unwrap();

    let engine = Engine::start(config(dir.path()), store).unwrap();
    let mut rx = engine.subscribe();
    wait_for(&mut rx, completed(id)).await;
}

#[tokio::test]
async fn browser_details_are_sent_on_every_request() {
    let srv =
        TestServer::start([("/f.bin", TestFile::new(pattern_bytes(LEN, 14)))], Faults::default())
            .await
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let mut rx = engine.subscribe();

    let id = engine
        .add(NewDownload {
            referrer: Some("https://shop.example/page".into()),
            user_agent: Some("Mozilla/5.0 (VDM test)".into()),
            cookies: Some("session=abc; theme=dark".into()),
            checksum: None,
            ..new_download(srv.url("/f.bin"))
        })
        .await
        .unwrap();
    wait_for(&mut rx, completed(id)).await;

    // Probe and every segment request carry the browser's cookies, referrer and agent.
    let n = srv.requests() as usize;
    assert!(n >= 2);
    assert_eq!(srv.header_values("cookie"), vec![Some("session=abc; theme=dark".to_owned()); n]);
    assert_eq!(srv.header_values("referer"), vec![Some("https://shop.example/page".to_owned()); n]);
    assert_eq!(srv.header_values("user-agent"), vec![Some("Mozilla/5.0 (VDM test)".to_owned()); n]);
    assert_eq!(record(&engine, id).await.headers.len(), 3, "kept for resuming");
}

// ------------------------------------------- checksums and after-download

async fn sha256_of(data: &[u8]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x");
    std::fs::write(&path, data).unwrap();
    vdm_core::checksum::hash_file(&path, vdm_core::checksum::Algorithm::Sha256).await.unwrap()
}

#[tokio::test]
async fn matching_checksum_completes_after_verifying() {
    let data = pattern_bytes(LEN, 20);
    let srv = TestServer::start([("/f.bin", TestFile::new(data.clone()))], Faults::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let mut rx = engine.subscribe();

    let hex = sha256_of(&data).await;
    let id = engine
        .add(NewDownload {
            checksum: Some(format!("SHA256:{}", hex.to_uppercase())),
            ..new_download(srv.url("/f.bin"))
        })
        .await
        .unwrap();
    wait_for(&mut rx, state(id, DownloadState::Verifying)).await;
    wait_for(&mut rx, completed(id)).await;
    assert_eq!(
        record(&engine, id).await.checksum.as_deref(),
        Some(format!("sha256:{hex}").as_str()),
        "stored normalized"
    );
}

#[tokio::test]
async fn checksum_mismatch_fails_but_keeps_the_file() {
    let srv =
        TestServer::start([("/f.bin", TestFile::new(pattern_bytes(LEN, 21)))], Faults::default())
            .await
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let mut rx = engine.subscribe();

    let wrong = format!("md5:{}", "0".repeat(32));
    let id = engine
        .add(NewDownload { checksum: Some(wrong), ..new_download(srv.url("/f.bin")) })
        .await
        .unwrap();
    let ev = wait_for(&mut rx, |ev| matches!(ev, Event::Failed { .. })).await;
    let Event::Failed { error, .. } = ev else { unreachable!() };
    assert!(error.contains("checksum mismatch") && error.contains("md5:"), "{error}");
    assert!(dir.path().join("f.bin").exists(), "the file is kept for the user to inspect");
    assert_eq!(record(&engine, id).await.state, DownloadState::Failed);
}

#[tokio::test]
async fn invalid_checksums_are_rejected_when_adding() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let err = engine
        .add(NewDownload {
            checksum: Some("sha256:nothex".into()),
            ..new_download("https://example.com/a".into())
        })
        .await
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(ref m) if m.contains("checksum")), "{err}");
}

/// A command that exits with `code` on every platform.
fn exit_with(code: u8) -> String {
    if cfg!(windows) { format!("cmd /C exit {code}") } else { format!("sh -c \"exit {code}\"") }
}

#[tokio::test]
async fn after_download_command_success_and_failure() {
    let files = ["/a.bin", "/b.bin"].map(|p| (p, TestFile::new(pattern_bytes(10_000, 22))));
    let srv = TestServer::start(files, Faults::default()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.max_active = 1;
    cfg.after_download = Some(exit_with(0));
    let engine = engine(cfg);
    let mut rx = engine.subscribe();

    let a = engine.add(new_download(srv.url("/a.bin"))).await.unwrap();
    wait_for(&mut rx, completed(a)).await;

    engine.set_after_download(Some(exit_with(3))).await.unwrap();
    let b = engine.add(new_download(srv.url("/b.bin"))).await.unwrap();
    let ev = wait_for(&mut rx, |ev| matches!(ev, Event::Failed { id, .. } if *id == b)).await;
    let Event::Failed { error, .. } = ev else { unreachable!() };
    assert!(error.contains("exit code 3"), "{error}");
    assert!(dir.path().join("b.bin").exists());
}

// ---------------------------------------------------------------- queues

fn queue_spec(name: &str) -> vdm_proto::QueueSpec {
    vdm_proto::QueueSpec {
        name: name.into(),
        max_active: None,
        speed_limit: None,
        schedule: None,
        when_done: vdm_proto::WhenDone::Nothing,
    }
}

fn in_queue(url: String, queue: QueueId) -> NewDownload {
    NewDownload { queue: Some(queue), ..new_download(url) }
}

#[tokio::test]
async fn the_main_queue_is_always_there() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let queues = engine.queues().await.unwrap();
    assert_eq!(queues.len(), 1);
    assert_eq!(queues[0].id, QueueId::MAIN);
    assert!(queues[0].in_window && !queues[0].stopped);
    assert!(engine.delete_queue(QueueId::MAIN).await.is_err());
}

#[tokio::test]
async fn a_queue_limits_its_own_downloads() {
    let files = ["/a.bin", "/b.bin", "/c.bin"].map(|p| (p, TestFile::new(pattern_bytes(LEN, 31))));
    let faults = Faults { throttle_bps: Some(400_000), ..Faults::default() };
    let srv = TestServer::start(files, faults).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path())); // three at once overall
    let one = engine
        .add_queue(vdm_proto::QueueSpec { max_active: Some(1), ..queue_spec("One") })
        .await
        .unwrap();
    let mut rx = engine.subscribe();

    let mut ids = Vec::new();
    for p in ["/a.bin", "/b.bin", "/c.bin"] {
        ids.push(engine.add(in_queue(srv.url(p), one)).await.unwrap());
    }
    let (mut running, mut max_running, mut done) = (0i32, 0, 0);
    wait_for(&mut rx, |ev| {
        match ev {
            Event::StateChanged { state: DownloadState::Downloading, .. } => running += 1,
            Event::StateChanged { state: DownloadState::Completed, .. } => {
                running -= 1;
                done += 1;
            }
            _ => {}
        }
        max_running = max_running.max(running);
        done == 3
    })
    .await;
    assert_eq!(max_running, 1, "the queue runs one at a time although three are allowed overall");
}

#[tokio::test]
async fn stopping_a_queue_puts_its_downloads_back_and_starting_resumes() {
    let srv = TestServer::start([("/f.bin", TestFile::new(pattern_bytes(LEN, 32)))], slow())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let q = engine.add_queue(queue_spec("Work")).await.unwrap();
    let mut rx = engine.subscribe();

    let id = engine.add(in_queue(srv.url("/f.bin"), q)).await.unwrap();
    wait_for(&mut rx, progressed(id, 10_000)).await;
    engine.set_queue_stopped(q, true).await.unwrap();
    wait_for(&mut rx, state(id, DownloadState::Queued)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(record(&engine, id).await.state, DownloadState::Queued, "stays waiting");
    let report = engine.queues().await.unwrap().into_iter().find(|x| x.id == q).unwrap();
    assert!(report.stopped);

    srv.set_faults(Faults::default());
    engine.set_queue_stopped(q, false).await.unwrap();
    wait_for(&mut rx, completed(id)).await;
}

#[tokio::test]
async fn a_queue_schedule_holds_downloads_until_its_window() {
    let srv =
        TestServer::start([("/f.bin", TestFile::new(pattern_bytes(LEN, 33)))], Faults::default())
            .await
            .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.clock = noon();
    let engine = engine(cfg);
    let night = vdm_proto::ScheduleSpec {
        start: Some("23:00".into()),
        stop: Some("07:00".into()),
        days: Vec::new(),
    };
    let q = engine
        .add_queue(vdm_proto::QueueSpec { schedule: Some(night), ..queue_spec("Night") })
        .await
        .unwrap();
    assert!(!engine.queues().await.unwrap().iter().find(|x| x.id == q).unwrap().in_window);
    let mut rx = engine.subscribe();

    let held = engine.add(in_queue(srv.url("/f.bin"), q)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(record(&engine, held).await.state, DownloadState::Queued);

    // Without the schedule it runs.
    engine.update_queue(q, queue_spec("Night")).await.unwrap();
    wait_for(&mut rx, completed(held)).await;
}

#[tokio::test]
async fn invalid_queue_settings_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    assert!(engine.add_queue(queue_spec("  ")).await.is_err());
    assert!(
        engine
            .add_queue(vdm_proto::QueueSpec { max_active: Some(0), ..queue_spec("Zero") })
            .await
            .is_err()
    );
    let bad = vdm_proto::ScheduleSpec { start: Some("9am".into()), ..Default::default() };
    assert!(
        engine
            .add_queue(vdm_proto::QueueSpec { schedule: Some(bad), ..queue_spec("Bad") })
            .await
            .is_err()
    );
    assert!(engine.add(in_queue("https://example.com/a".into(), QueueId(77))).await.is_err());
}

#[tokio::test]
async fn moving_into_a_stopped_queue_waits_and_deleting_a_queue_moves_to_main() {
    let srv = TestServer::start([("/f.bin", TestFile::new(pattern_bytes(LEN, 34)))], slow())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let parked = engine.add_queue(queue_spec("Parked")).await.unwrap();
    engine.set_queue_stopped(parked, true).await.unwrap();
    let mut rx = engine.subscribe();

    let id = engine.add(new_download(srv.url("/f.bin"))).await.unwrap();
    wait_for(&mut rx, progressed(id, 10_000)).await;
    engine.command(Command::MoveToQueue { id, queue: parked }).await.unwrap();
    wait_for(&mut rx, state(id, DownloadState::Queued)).await;
    assert_eq!(record(&engine, id).await.queue, parked);

    srv.set_faults(Faults::default());
    engine.delete_queue(parked).await.unwrap();
    let mut moved = false;
    let mut removed = false;
    wait_for(&mut rx, |ev| {
        match ev {
            Event::Moved { id: i, queue } if *i == id && *queue == QueueId::MAIN => moved = true,
            Event::QueueRemoved { id: q } if *q == parked => removed = true,
            _ => {}
        }
        moved && removed
    })
    .await;
    wait_for(&mut rx, completed(id)).await;
    assert_eq!(record(&engine, id).await.queue, QueueId::MAIN);
}

#[tokio::test]
async fn a_queue_reports_when_it_has_finished() {
    let files = ["/a.bin", "/b.bin"].map(|p| (p, TestFile::new(pattern_bytes(LEN, 35))));
    let srv = TestServer::start(files, Faults::default()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(config(dir.path()));
    let q = engine
        .add_queue(vdm_proto::QueueSpec {
            when_done: vdm_proto::WhenDone::ShutDown,
            ..queue_spec("Tonight")
        })
        .await
        .unwrap();
    let mut rx = engine.subscribe();
    let a = engine.add(in_queue(srv.url("/a.bin"), q)).await.unwrap();
    let b = engine.add(in_queue(srv.url("/b.bin"), q)).await.unwrap();

    let mut completed_before = 0;
    let finished = wait_for(&mut rx, |ev| {
        if matches!(ev, Event::Completed { id, .. } if *id == a || *id == b) {
            completed_before += 1;
        }
        matches!(ev, Event::QueueFinished { .. })
    })
    .await;
    assert_eq!(completed_before, 2, "only after both are done");
    assert_eq!(finished, Event::QueueFinished { id: q, when_done: vdm_proto::WhenDone::ShutDown });
}
