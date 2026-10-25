//! Checks the test server's HTTP behavior with a real client, so later engine
//! tests can rely on it.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, RANGE};
use vdm_testsrv::{Faults, TestFile, TestServer, pattern_bytes};

const LEN: usize = 100_000;

async fn server(faults: Faults) -> (TestServer, bytes::Bytes) {
    let data = pattern_bytes(LEN, 42);
    let srv = TestServer::start([("/f.bin", TestFile::new(data.clone()))], faults).await.unwrap();
    (srv, data)
}

#[tokio::test]
async fn full_get() {
    let (srv, data) = server(Faults::default()).await;
    let resp = reqwest::get(srv.url("/f.bin")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[ACCEPT_RANGES], "bytes");
    assert_eq!(resp.headers()[CONTENT_LENGTH], LEN.to_string());
    assert_eq!(resp.bytes().await.unwrap(), data);
}

#[tokio::test]
async fn partial_get() {
    let (srv, data) = server(Faults::default()).await;
    let client = reqwest::Client::new();
    let resp = client.get(srv.url("/f.bin")).header(RANGE, "bytes=1000-1999").send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers()[CONTENT_RANGE], format!("bytes 1000-1999/{LEN}"));
    assert_eq!(resp.bytes().await.unwrap(), data.slice(1000..2000));
    assert_eq!(srv.range_headers(), vec![Some("bytes=1000-1999".to_owned())]);
}

#[tokio::test]
async fn unsatisfiable_range() {
    let (srv, _) = server(Faults::default()).await;
    let resp = reqwest::Client::new()
        .get(srv.url("/f.bin"))
        .header(RANGE, format!("bytes={LEN}-"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(resp.headers()[CONTENT_RANGE], format!("bytes */{LEN}"));
}

#[tokio::test]
async fn head_reports_length_without_body() {
    let (srv, _) = server(Faults::default()).await;
    let resp = reqwest::Client::new().head(srv.url("/f.bin")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[CONTENT_LENGTH], LEN.to_string());
    assert!(resp.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn if_range_mismatch_serves_full_file() {
    let (srv, data) = server(Faults::default()).await;
    let client = reqwest::Client::new();
    let etag = client.get(srv.url("/f.bin")).send().await.unwrap().headers()[ETAG].clone();

    // Matching validator: range honored.
    let resp = client
        .get(srv.url("/f.bin"))
        .header(RANGE, "bytes=10-19")
        .header(IF_RANGE, etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);

    // File changed on the server: stale validator gets the full new file.
    let new = pattern_bytes(LEN, 7);
    srv.set_file("/f.bin", TestFile::new(new.clone()).with_etag("\"v2\""));
    let resp = client
        .get(srv.url("/f.bin"))
        .header(RANGE, "bytes=10-19")
        .header(IF_RANGE, "\"len-100000\"")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.bytes().await.unwrap();
    assert_eq!(body, new);
    assert_ne!(body, data);
}

#[tokio::test]
async fn no_ranges_fault_ignores_range() {
    let (srv, data) = server(Faults { no_ranges: true, ..Faults::default() }).await;
    let resp = reqwest::Client::new()
        .get(srv.url("/f.bin"))
        .header(RANGE, "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get(ACCEPT_RANGES).is_none());
    assert_eq!(resp.bytes().await.unwrap(), data);
}

#[tokio::test]
async fn cut_after_fault_aborts_body() {
    let (srv, data) = server(Faults { cut_after: Some(20_000), ..Faults::default() }).await;
    let mut resp = reqwest::get(srv.url("/f.bin")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut received = Vec::new();
    let err = loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => received.extend_from_slice(&chunk),
            Ok(None) => panic!("body should end early with an error"),
            Err(err) => break err,
        }
    };
    assert!(err.is_body() || err.is_decode(), "unexpected error kind: {err}");
    assert_eq!(received, data[..20_000]);
}

#[tokio::test]
async fn fail_first_fault_returns_503_then_recovers() {
    let (srv, _) = server(Faults { fail_first: 2, ..Faults::default() }).await;
    for _ in 0..2 {
        let resp = reqwest::get(srv.url("/f.bin")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers()["retry-after"], "1");
    }
    let resp = reqwest::get(srv.url("/f.bin")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(srv.requests(), 3);
}

#[tokio::test]
async fn hide_length_fault_omits_content_length() {
    let (srv, data) = server(Faults { hide_length: true, ..Faults::default() }).await;
    let resp = reqwest::get(srv.url("/f.bin")).await.unwrap();
    assert!(resp.headers().get(CONTENT_LENGTH).is_none());
    assert_eq!(resp.bytes().await.unwrap(), data);
}

#[tokio::test]
async fn missing_file_is_404() {
    let (srv, _) = server(Faults::default()).await;
    let resp = reqwest::get(srv.url("/nope")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Poll until no body is in flight, failing if that takes longer than `timeout`.
async fn wait_for_idle(srv: &TestServer, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while srv.in_flight() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "still {} bodies in flight after {timeout:?}",
            srv.in_flight()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn in_flight_counts_parallel_bodies() {
    let data = pattern_bytes(200_000, 5);
    let faults = Faults { throttle_bps: Some(200_000), ..Faults::default() };
    let srv = TestServer::start([("/big.bin", TestFile::new(data.clone()))], faults).await.unwrap();
    let client = reqwest::Client::new();
    let url = srv.url("/big.bin");

    let bodies = futures_util::future::join_all((0..4).map(|_| {
        let client = client.clone();
        let url = url.clone();
        async move { client.get(url).send().await.unwrap().bytes().await.unwrap() }
    }))
    .await;

    for body in bodies {
        assert_eq!(body, data);
    }
    assert_eq!(srv.max_in_flight(), 4);
    wait_for_idle(&srv, Duration::from_secs(1)).await;
    assert_eq!(srv.in_flight(), 0);
}

#[tokio::test]
async fn in_flight_drops_when_client_disconnects() {
    let data = pattern_bytes(1_000_000, 8);
    let faults = Faults { throttle_bps: Some(200_000), ..Faults::default() };
    let srv = TestServer::start([("/big.bin", TestFile::new(data))], faults).await.unwrap();

    let mut resp = reqwest::get(srv.url("/big.bin")).await.unwrap();
    assert_eq!(srv.in_flight(), 1);
    assert!(resp.chunk().await.unwrap().is_some());
    drop(resp);

    // The server only notices the closed socket on its next write, which is
    // throttled, so allow a few chunk intervals.
    wait_for_idle(&srv, Duration::from_secs(3)).await;
    assert_eq!(srv.in_flight(), 0);
}

#[tokio::test]
async fn cut_every_cuts_only_some_responses() {
    let faults = Faults { cut_after: Some(20_000), cut_every: Some(2), ..Faults::default() };
    let (srv, data) = server(faults).await;

    // Response 1: not cut.
    let resp = reqwest::get(srv.url("/f.bin")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap(), data);

    // Response 2: cut after 20_000 bytes.
    let mut resp = reqwest::get(srv.url("/f.bin")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut received = Vec::new();
    let err = loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => received.extend_from_slice(&chunk),
            Ok(None) => panic!("body should end early with an error"),
            Err(err) => break err,
        }
    };
    assert!(err.is_body() || err.is_decode(), "unexpected error kind: {err}");
    assert_eq!(received, data[..20_000]);

    // Response 3: not cut.
    let resp = reqwest::get(srv.url("/f.bin")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap(), data);
    assert_eq!(srv.requests(), 3);
}

#[tokio::test]
async fn max_concurrent_rejects_extra_connections() {
    let data = pattern_bytes(1_000_000, 6);
    let faults =
        Faults { throttle_bps: Some(200_000), max_concurrent: Some(2), ..Faults::default() };
    let srv = TestServer::start([("/big.bin", TestFile::new(data.clone()))], faults).await.unwrap();
    let client = reqwest::Client::new();
    let url = srv.url("/big.bin");

    // Two bodies in flight, each confirmed by reading a chunk.
    let mut first = client.get(url.as_str()).send().await.unwrap();
    let mut second = client.get(url.as_str()).send().await.unwrap();
    assert!(first.chunk().await.unwrap().is_some());
    assert!(second.chunk().await.unwrap().is_some());
    assert_eq!(srv.in_flight(), 2);

    // A third body is refused, and the refusal does not count as in flight.
    let refused = client.get(url.as_str()).send().await.unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.headers()["retry-after"], "1");
    assert_eq!(srv.in_flight(), 2);
    assert_eq!(srv.max_in_flight(), 2);

    // Once the first two are gone, a new body is admitted.
    drop(first);
    drop(second);
    wait_for_idle(&srv, Duration::from_secs(3)).await;
    let resp = client.get(url.as_str()).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(srv.in_flight(), 1);
}
