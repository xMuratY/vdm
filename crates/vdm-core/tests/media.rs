//! HLS/DASH downloads against vdm-testsrv: plain, encrypted, byte ranges,
//! fMP4, separate audio, DASH templates, quality choice, resume, errors,
//! the engine, and (when ffmpeg is installed) a real stream muxed to MP4.

use std::path::Path;
use std::time::Duration;

use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use bytes::Bytes;
use vdm_core::{
    CancellationToken, DownloadRequest, Downloader, Error, Options, Progress, Quality, RetryPolicy,
};
use vdm_testsrv::{Faults, TestFile, TestServer, pattern_bytes};

const SEG: usize = 30_000;

fn downloader_with(f: impl FnOnce(&mut Options)) -> Downloader {
    let mut opts = Options {
        retry: RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        },
        media_concurrency: 3,
        remux: false,
        ..Options::default()
    };
    f(&mut opts);
    Downloader::new(opts).unwrap()
}

fn downloader() -> Downloader {
    downloader_with(|_| {})
}

fn file(bytes: impl Into<Bytes>) -> TestFile {
    TestFile::new(bytes)
}

fn text(s: String) -> TestFile {
    TestFile::new(Bytes::from(s))
}

/// `n` segment payloads, distinct per seed.
fn payloads(n: usize, seed: u64) -> Vec<Bytes> {
    (0..n).map(|i| pattern_bytes(SEG + i * 7, seed * 100 + i as u64)).collect()
}

fn concat(parts: &[Bytes]) -> Vec<u8> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

fn media_playlist(names: &[String], extra_head: &str) -> String {
    let mut s = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n{extra_head}");
    for name in names {
        s.push_str(&format!("#EXTINF:4.0,\n{name}\n"));
    }
    s.push_str("#EXT-X-ENDLIST\n");
    s
}

async fn get(dl: &Downloader, srv: &TestServer, path: &str, dir: &Path) -> vdm_core::Completed {
    dl.download(
        DownloadRequest::new(srv.url(path), dir),
        |_: Progress| {},
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn plain_hls_is_joined_into_one_ts() {
    let parts = payloads(5, 1);
    let names: Vec<String> = (0..5).map(|i| format!("seg{i}.ts")).collect();
    let mut files: Vec<(String, TestFile)> =
        names.iter().zip(&parts).map(|(n, p)| (format!("/show/{n}"), file(p.clone()))).collect();
    files.push(("/show/episode-1.m3u8".into(), text(media_playlist(&names, ""))));
    let srv = TestServer::start(files, Faults::default()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let mut last = None;
    let done = downloader()
        .download(
            DownloadRequest::new(srv.url("/show/episode-1.m3u8"), dir.path()),
            |p: Progress| last = Some(p),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(done.path, dir.path().join("episode-1.ts"));
    assert_eq!(std::fs::read(&done.path).unwrap(), concat(&parts));
    assert_eq!(listing(dir.path()), ["episode-1.ts"], "work folder and sidecar removed");
    let last = last.unwrap();
    assert_eq!((last.downloaded, last.total), (done.size, Some(done.size)));
}

#[tokio::test]
async fn master_playlist_quality_choice_and_folder_name() {
    let low = payloads(3, 2);
    let high = payloads(3, 3);
    let mut files: Vec<(String, TestFile)> = Vec::new();
    for (q, parts) in [("360", &low), ("1080", &high)] {
        let names: Vec<String> = (0..3).map(|i| format!("{q}_{i}.ts")).collect();
        for (n, p) in names.iter().zip(parts.iter()) {
            files.push((format!("/movies/bunny/{n}"), file(p.clone())));
        }
        files.push((format!("/movies/bunny/{q}.m3u8"), text(media_playlist(&names, ""))));
    }
    let master = "#EXTM3U\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n360.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080\n1080.m3u8\n";
    files.push(("/movies/bunny/master.m3u8".into(), text(master.into())));
    let srv = TestServer::start(files, Faults::default()).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let best = get(&downloader(), &srv, "/movies/bunny/master.m3u8", dir.path()).await;
    assert_eq!(best.path.file_name().unwrap(), "bunny.ts", "generic master.m3u8 → folder name");
    assert_eq!(std::fs::read(&best.path).unwrap(), concat(&high));

    let mut req = DownloadRequest::new(srv.url("/movies/bunny/master.m3u8"), dir.path());
    req.quality = Quality::Worst;
    let worst =
        downloader().download(req, |_: Progress| {}, CancellationToken::new()).await.unwrap();
    assert_eq!(worst.path.file_name().unwrap(), "bunny (1).ts");
    assert_eq!(std::fs::read(&worst.path).unwrap(), concat(&low));

    let info = downloader()
        .probe_media(
            &srv.url("/movies/bunny/master.m3u8"),
            &[],
            Quality::MaxHeight(720),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(info.variants.len(), 2);
    assert_eq!(info.chosen.map(|i| info.variants[i].height), Some(Some(360)));
}

fn encrypt(plain: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Bytes {
    let mut buf = plain.to_vec();
    buf.resize(plain.len() + 16, 0);
    let len = cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
        .unwrap()
        .len();
    buf.truncate(len);
    buf.into()
}

#[tokio::test]
async fn aes128_segments_are_decrypted_with_explicit_and_implicit_ivs() {
    let parts = payloads(4, 4);
    let key = [0x42u8; 16];
    let explicit_iv = [0x11u8; 16];
    let mut files: Vec<(String, TestFile)> = vec![("/enc/key.bin".into(), file(key.to_vec()))];
    // Segments 0-1 use an explicit IV; 2-3 use the media sequence number (starts at 7).
    for (i, p) in parts.iter().enumerate() {
        let iv = if i < 2 {
            explicit_iv
        } else {
            let mut iv = [0u8; 16];
            iv[8..].copy_from_slice(&(7 + i as u64).to_be_bytes());
            iv
        };
        files.push((format!("/enc/s{i}.ts"), file(encrypt(p, &key, &iv))));
    }
    let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:7\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x11111111111111111111111111111111\n\
        #EXTINF:4,\ns0.ts\n#EXTINF:4,\ns1.ts\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
        #EXTINF:4,\ns2.ts\n#EXTINF:4,\ns3.ts\n#EXT-X-ENDLIST\n";
    files.push(("/enc/secret.m3u8".into(), text(playlist.into())));
    let srv = TestServer::start(files, Faults::default()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let done = get(&downloader(), &srv, "/enc/secret.m3u8", dir.path()).await;
    assert_eq!(std::fs::read(&done.path).unwrap(), concat(&parts));
}

#[tokio::test]
async fn byte_ranges_fmp4_init_and_separate_audio() {
    // Video: one fMP4 resource addressed by byte ranges, with an init section.
    let init = pattern_bytes(500, 50);
    let vparts = payloads(3, 5);
    let mut blob = init.to_vec();
    let mut ranges = Vec::new();
    for p in &vparts {
        ranges.push((blob.len(), p.len()));
        blob.extend_from_slice(p);
    }
    let mut video = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-VERSION:7\n\
        #EXT-X-MAP:URI=\"video.mp4\",BYTERANGE=\"500@0\"\n"
        .to_owned();
    for (off, len) in &ranges {
        video.push_str(&format!("#EXTINF:4,\n#EXT-X-BYTERANGE:{len}@{off}\nvideo.mp4\n"));
    }
    video.push_str("#EXT-X-ENDLIST\n");
    // Audio: separate rendition, plain fMP4 segments.
    let ainit = pattern_bytes(200, 60);
    let aparts = payloads(3, 6);
    let anames: Vec<String> = (0..3).map(|i| format!("a{i}.m4s")).collect();
    let audio = media_playlist(&anames, "#EXT-X-MAP:URI=\"ainit.mp4\"\n");
    let master = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,AUDIO=\"aud\"\nvideo.m3u8\n";
    let mut files: Vec<(String, TestFile)> = vec![
        ("/talk/video.mp4".into(), file(blob)),
        ("/talk/video.m3u8".into(), text(video)),
        ("/talk/ainit.mp4".into(), file(ainit.clone())),
        ("/talk/audio.m3u8".into(), text(audio)),
        ("/talk/master.m3u8".into(), text(master.into())),
    ];
    for (n, p) in anames.iter().zip(&aparts) {
        files.push((format!("/talk/{n}"), file(p.clone())));
    }
    let srv = TestServer::start(files, Faults::default()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let done = get(&downloader(), &srv, "/talk/master.m3u8", dir.path()).await;
    // No muxing in this test: video gets the name, audio sits beside it.
    assert_eq!(listing(dir.path()), ["talk.m4a", "talk.mp4"]);
    let mut want_video = init.to_vec();
    want_video.extend(concat(&vparts));
    assert_eq!(std::fs::read(&done.path).unwrap(), want_video);
    let mut want_audio = ainit.to_vec();
    want_audio.extend(concat(&aparts));
    assert_eq!(std::fs::read(dir.path().join("talk.m4a")).unwrap(), want_audio);
}

#[tokio::test]
async fn dash_segment_template_video_and_audio() {
    let vinit = pattern_bytes(300, 70);
    let ainit = pattern_bytes(100, 71);
    let v = payloads(4, 7);
    let a = payloads(4, 8);
    let mpd = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT16S" minBufferTime="PT2S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/seg-$Number$.m4s" startNumber="1" duration="4" timescale="1"/>
      <Representation id="v720" bandwidth="2000000" width="1280" height="720" codecs="avc1.64001f"/>
      <Representation id="v360" bandwidth="600000" width="640" height="360" codecs="avc1.4d401e"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/seg-$Number$.m4s" startNumber="1" duration="4" timescale="1"/>
      <Representation id="aud" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let mut files: Vec<(String, TestFile)> = vec![
        ("/dash/lecture/manifest.mpd".into(), text(mpd.into())),
        ("/dash/lecture/v720/init.mp4".into(), file(vinit.clone())),
        ("/dash/lecture/aud/init.mp4".into(), file(ainit.clone())),
    ];
    for i in 0..4 {
        files.push((format!("/dash/lecture/v720/seg-{}.m4s", i + 1), file(v[i].clone())));
        files.push((format!("/dash/lecture/aud/seg-{}.m4s", i + 1), file(a[i].clone())));
    }
    let srv = TestServer::start(files, Faults::default()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let done = get(&downloader(), &srv, "/dash/lecture/manifest.mpd", dir.path()).await;
    assert_eq!(listing(dir.path()), ["lecture.m4a", "lecture.mp4"]);
    let mut want = vinit.to_vec();
    want.extend(concat(&v));
    assert_eq!(std::fs::read(&done.path).unwrap(), want);
    let mut want_audio = ainit.to_vec();
    want_audio.extend(concat(&a));
    assert_eq!(std::fs::read(dir.path().join("lecture.m4a")).unwrap(), want_audio);
}

#[tokio::test]
async fn cancel_then_resume_skips_finished_segments() {
    let parts = payloads(8, 9);
    let names: Vec<String> = (0..8).map(|i| format!("s{i}.ts")).collect();
    let mut files: Vec<(String, TestFile)> =
        names.iter().zip(&parts).map(|(n, p)| (format!("/r/{n}"), file(p.clone()))).collect();
    files.push(("/r/clip.m3u8".into(), text(media_playlist(&names, ""))));
    // ~0.3 s per segment, 2 at a time: cancel after a few are done.
    let faults = Faults { throttle_bps: Some(100_000), ..Faults::default() };
    let srv = TestServer::start(files, faults).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let dl = downloader_with(|o| o.media_concurrency = 2);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let err = dl
        .download(
            DownloadRequest::new(srv.url("/r/clip.m3u8"), dir.path()),
            move |p: Progress| {
                if p.downloaded >= 3 * SEG as u64 {
                    trigger.cancel();
                }
            },
            cancel,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Cancelled { .. }), "got {err}");
    assert!(dir.path().join("clip.ts.vdmpart").is_dir(), "segments kept for resuming");

    srv.set_faults(Faults::default());
    let before = srv.requests();
    let done = get(&dl, &srv, "/r/clip.m3u8", dir.path()).await;
    assert!(done.resumed_from >= 3 * SEG as u64, "resumed from {}", done.resumed_from);
    assert_eq!(std::fs::read(&done.path).unwrap(), concat(&parts));
    let fetched = srv.requests() - before;
    assert!(fetched < 1 + 8, "only missing segments fetched again ({fetched} requests)");
}

#[tokio::test]
async fn live_streams_and_missing_segments_fail_clearly() {
    let live = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\na.ts\n#EXTINF:4,\nb.ts\n";
    let broken = media_playlist(&["ok.ts".into(), "missing.ts".into()], "");
    let srv = TestServer::start(
        [
            ("/live.m3u8".to_owned(), text(live.into())),
            ("/broken.m3u8".to_owned(), text(broken)),
            ("/ok.ts".to_owned(), file(pattern_bytes(100, 1))),
        ],
        Faults::default(),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dl = downloader();
    let run = |path: &str| {
        dl.download(
            DownloadRequest::new(srv.url(path), dir.path()),
            |_: Progress| {},
            CancellationToken::new(),
        )
    };
    let err = run("/live.m3u8").await.unwrap_err();
    assert!(err.to_string().contains("live"), "{err}");
    let err = run("/broken.m3u8").await.unwrap_err();
    assert!(matches!(err, Error::Status { status, .. } if status == 404), "{err}");
}

#[tokio::test]
async fn engine_downloads_streams_too() {
    use vdm_core::{Engine, EngineConfig};
    use vdm_proto::{Event, NewDownload};

    let parts = payloads(3, 10);
    let names: Vec<String> = (0..3).map(|i| format!("s{i}.ts")).collect();
    let mut files: Vec<(String, TestFile)> =
        names.iter().zip(&parts).map(|(n, p)| (format!("/e/{n}"), file(p.clone()))).collect();
    files.push(("/e/news.m3u8".into(), text(media_playlist(&names, ""))));
    let srv = TestServer::start(files, Faults::default()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = EngineConfig::new(dir.path());
    cfg.options.remux = false;
    let engine = Engine::start(cfg, vdm_store::Store::open_in_memory().unwrap()).unwrap();
    let mut rx = engine.subscribe();
    let id = engine
        .add(NewDownload {
            url: srv.url("/e/news.m3u8"),
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
    assert!(path.ends_with("news.ts"), "{path}");
    assert_eq!(std::fs::read(&path).unwrap(), concat(&parts));
}

/// A real H.264/AAC stream made by ffmpeg (encrypted HLS), muxed back to MP4.
#[tokio::test]
async fn real_stream_is_muxed_to_mp4_with_ffmpeg() {
    let Some(ffmpeg) = which("ffmpeg") else {
        eprintln!("ffmpeg not found; skipping");
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let key = [0x5au8; 16];
    std::fs::write(src.path().join("enc.key"), key).unwrap();
    std::fs::write(
        src.path().join("keyinfo.txt"),
        format!("enc.key\n{}\n", src.path().join("enc.key").display()),
    )
    .unwrap();
    let status = std::process::Command::new(&ffmpeg)
        .current_dir(src.path())
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=6:size=320x240:rate=15",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=6",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "15",
            "-c:a",
            "aac",
            "-shortest",
            "-f",
            "hls",
            "-hls_time",
            "2",
            "-hls_playlist_type",
            "vod",
            "-hls_key_info_file",
            "keyinfo.txt",
            "-hls_segment_filename",
            "seg%d.ts",
            "demo.m3u8",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg could not make the test stream");

    let mut files = Vec::new();
    for entry in std::fs::read_dir(src.path()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        files.push((format!("/real/{name}"), file(std::fs::read(&path).unwrap())));
    }
    let srv = TestServer::start(files, Faults::default()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dl = downloader_with(|o| o.remux = true);
    let done = get(&dl, &srv, "/real/demo.m3u8", dir.path()).await;
    assert_eq!(done.path.file_name().unwrap(), "demo.mp4");

    // ffprobe confirms a playable MP4 with one video and one audio stream, ~6 s long.
    let ffprobe = ffmpeg.with_file_name(format!("ffprobe{}", std::env::consts::EXE_SUFFIX));
    let out = std::process::Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type:format=duration,format_name",
            "-of",
            "csv=p=0",
        ])
        .arg(&done.path)
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{report}");
    assert!(report.contains("video") && report.contains("audio"), "{report}");
    assert!(report.contains("mp4"), "{report}");
    let duration: f64 = report
        .lines()
        .find_map(|l| l.split(',').find_map(|f| f.parse::<f64>().ok()))
        .unwrap_or(0.0);
    assert!((5.0..7.5).contains(&duration), "duration {duration} in {report}");
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&exe)).find(|p| p.is_file())
}

#[tokio::test]
async fn cookies_stay_with_the_stream_host() {
    // Playlist on one server, segments on another ("CDN").
    let parts = payloads(2, 11);
    let cdn = TestServer::start(
        [
            ("/a.ts".to_owned(), file(parts[0].clone())),
            ("/b.ts".to_owned(), file(parts[1].clone())),
        ],
        Faults::default(),
    )
    .await
    .unwrap();
    let names = [cdn.url("/a.ts"), cdn.url("/b.ts")];
    let site = TestServer::start(
        [("/watch/show.m3u8".to_owned(), text(media_playlist(&names, "")))],
        Faults::default(),
    )
    .await
    .unwrap();
    // Different ports are the same host, so give the CDN a different host name.
    let playlist = site.url("/watch/show.m3u8").replace("127.0.0.1", "localhost");
    let dir = tempfile::tempdir().unwrap();
    let mut req = DownloadRequest::new(playlist, dir.path());
    req.headers = vec![("Cookie".into(), "session=abc".into())];
    let done =
        downloader().download(req, |_: Progress| {}, CancellationToken::new()).await.unwrap();
    assert_eq!(std::fs::read(&done.path).unwrap(), concat(&parts));
    assert_eq!(site.header_values("cookie"), vec![Some("session=abc".to_owned())]);
    assert!(
        cdn.header_values("cookie").iter().all(Option::is_none),
        "{:?}",
        cdn.header_values("cookie")
    );
}
