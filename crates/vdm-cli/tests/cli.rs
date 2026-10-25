//! End-to-end checks of the `vdm` binary against vdm-testsrv.

use std::process::{Command, Output};

use vdm_testsrv::{Faults, TestFile, TestServer, pattern_bytes};

async fn vdm(args: Vec<String>) -> Output {
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_vdm")).args(args).output().unwrap()
    })
    .await
    .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn get_downloads_the_file() {
    let data = pattern_bytes(50_000, 1);
    let srv = TestServer::start([("/a.bin", TestFile::new(data.clone()))], Faults::default())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();

    let out = vdm(vec![
        "get".into(),
        srv.url("/a.bin"),
        "-o".into(),
        dir.path().display().to_string(),
        "-H".into(),
        "Referer: https://example.com/".into(),
    ])
    .await;
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout(&out).starts_with("Saved "), "stdout: {}", stdout(&out));
    assert_eq!(std::fs::read(dir.path().join("a.bin")).unwrap(), data);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_uses_requested_connections() {
    // 4 MiB with the default 1 MiB minimum segment: room for 4 connections.
    let data = pattern_bytes(4 << 20, 2);
    let faults = Faults { throttle_bps: Some(4 << 20), ..Faults::default() };
    let srv = TestServer::start([("/big.bin", TestFile::new(data.clone()))], faults).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let out = vdm(vec![
        "get".into(),
        srv.url("/big.bin"),
        "-o".into(),
        dir.path().display().to_string(),
        "-c".into(),
        "4".into(),
    ])
    .await;
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(dir.path().join("big.bin")).unwrap(), data);
    assert_eq!(srv.max_in_flight(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_prints_details() {
    let file = TestFile::new(pattern_bytes(2048, 1))
        .with_content_disposition(r#"attachment; filename="named.zip""#);
    let srv = TestServer::start([("/a.bin", file)], Faults::default()).await.unwrap();

    let out = vdm(vec!["probe".into(), srv.url("/a.bin")]).await;
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("File name:     named.zip"), "{text}");
    assert!(text.contains("Size:          2.0 KiB (2048 bytes)"), "{text}");
    assert!(text.contains("Resumable:     yes"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_url_fails() {
    let out = vdm(vec!["probe".into(), "file:///etc/passwd".into()]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid URL"));
}

#[tokio::test(flavor = "multi_thread")]
async fn get_verifies_checksums_and_sends_logins() {
    let data = pattern_bytes(40_000, 5);
    let srv = TestServer::start([("/c.bin", TestFile::new(data.clone()))], Faults::default())
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let probe_file = tmp.path().join("x");
    std::fs::write(&probe_file, &data).unwrap();
    let sha = vdm_core::checksum::hash_file(&probe_file, vdm_core::checksum::Algorithm::Sha256)
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let base = |checksum: String| {
        vec![
            "get".into(),
            srv.url("/c.bin"),
            "-o".into(),
            dir.path().display().to_string(),
            "--checksum".into(),
            checksum,
            "--login".into(),
            "ann:s3cret".into(),
        ]
    };
    let ok = vdm(base(format!("sha256:{sha}"))).await;
    assert!(ok.status.success(), "stderr: {}", String::from_utf8_lossy(&ok.stderr));
    assert!(stdout(&ok).contains("Checksum OK"), "{}", stdout(&ok));
    assert!(
        srv.header_values("authorization")
            .iter()
            .all(|a| a.as_deref() == Some("Basic YW5uOnMzY3JldA=="))
    );

    let bad = vdm(base(format!("sha256:{}", "0".repeat(64)))).await;
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("checksum mismatch"));
    assert!(dir.path().join("c (1).bin").exists(), "the file is kept");

    let invalid = vdm(base("sha256:xyz".into())).await;
    assert_eq!(invalid.status.code(), Some(2), "clap rejects a malformed checksum");
}
