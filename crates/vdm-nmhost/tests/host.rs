//! Runs the real host binary over its stdin/stdout, as a browser would.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;
use vdm_ipc::{Instance, Request};
use vdm_proto::NewDownload;

const BIN: &str = env!("CARGO_BIN_EXE_vdm-nmhost");
/// Longest wait for any single reply or exit.
const WAIT: Duration = Duration::from_secs(5);

/// A running host with its stdin and a reader for its replies.
struct Host {
    child: Child,
    stdin: Option<ChildStdin>,
    replies: Receiver<Value>,
}

impl Host {
    fn start(data_dir: &Path, gui_path: Option<&Path>) -> Host {
        let mut command = Command::new(BIN);
        command
            .args(["chrome-extension://abc/", "--parent-window=0"])
            .env("VDM_DATA_DIR", data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        match gui_path {
            Some(path) => command.env("VDM_GUI_PATH", path),
            None => command.env_remove("VDM_GUI_PATH"),
        };
        let mut child = command.spawn().expect("start the host");
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("host stdout");

        let (tx, replies) = mpsc::channel();
        thread::spawn(move || {
            while let Some(reply) = read_frame(&mut stdout) {
                if tx.send(reply).is_err() {
                    break;
                }
            }
        });
        Host { child, stdin, replies }
    }

    fn send_raw(&mut self, body: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(&(body.len() as u32).to_ne_bytes()).expect("write length");
        stdin.write_all(body).expect("write body");
        stdin.flush().expect("flush");
    }

    fn send(&mut self, message: &Value) {
        self.send_raw(&serde_json::to_vec(message).expect("encode"));
    }

    fn recv(&mut self) -> Value {
        self.replies.recv_timeout(WAIT).expect("a reply from the host")
    }

    fn request(&mut self, message: &Value) -> Value {
        self.send(message);
        self.recv()
    }

    /// Close stdin and wait for the host to exit.
    fn finish(mut self) -> ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll host") {
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("host did not exit after stdin closed");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Read one framed reply, or `None` when the host has closed its stdout.
fn read_frame(stdout: &mut impl Read) -> Option<Value> {
    let mut len = [0u8; 4];
    stdout.read_exact(&mut len).ok()?;
    let mut body = vec![0u8; u32::from_ne_bytes(len) as usize];
    stdout.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn download_message() -> Value {
    json!({
        "type": "download",
        "url": "https://example.com/files/setup.exe",
        "referrer": "https://example.com/",
        "cookies": "sid=abc123",
        "user_agent": "Mozilla/5.0 (test)",
        "file_name": "setup.exe",
    })
}

#[test]
fn ping_returns_the_version() {
    let dir = TempDir::new().unwrap();
    let mut host = Host::start(dir.path(), None);
    let reply = host.request(&json!({"type": "ping"}));
    assert_eq!(reply, json!({"type": "pong", "version": env!("CARGO_PKG_VERSION")}));
    assert!(host.finish().success());
}

#[test]
fn several_messages_in_one_session_get_replies_in_order() {
    let dir = TempDir::new().unwrap();
    let mut host = Host::start(dir.path(), None);
    host.send(&json!({"type": "ping"}));
    host.send(&json!({"type": "nope"}));
    host.send(&json!({"type": "ping"}));
    assert_eq!(host.recv()["type"], "pong");
    assert_eq!(host.recv()["ok"], false);
    assert_eq!(host.recv()["type"], "pong");
    assert!(host.finish().success());
}

#[test]
fn eof_before_any_message_exits_zero() {
    let dir = TempDir::new().unwrap();
    let host = Host::start(dir.path(), None);
    assert_eq!(host.finish().code(), Some(0));
}

#[test]
fn download_is_forwarded_to_the_running_gui() {
    let dir = TempDir::new().unwrap();
    let socket = vdm_ipc::socket_for(&vdm_ipc::database_path(Some(dir.path())).unwrap());

    // Play the GUI: this process owns the socket and records what arrives.
    let Instance::Primary(server) = vdm_ipc::acquire(&socket, &Request::default()).unwrap() else {
        panic!("expected to become the GUI");
    };
    let (tx, rx) = mpsc::channel();
    server.serve(move |request| {
        let _ = tx.send(request);
    });

    let mut host = Host::start(dir.path(), None);
    let reply = host.request(&download_message());
    assert_eq!(reply, json!({"type": "result", "ok": true}));
    assert!(host.finish().success());

    let request = rx.recv_timeout(WAIT).expect("the GUI received the download");
    assert_eq!(
        request,
        Request {
            urls: vec![],
            show: true,
            downloads: vec![NewDownload {
                url: "https://example.com/files/setup.exe".into(),
                save_dir: None,
                file_name: Some("setup.exe".into()),
                connections: None,
                referrer: Some("https://example.com/".into()),
                user_agent: Some("Mozilla/5.0 (test)".into()),
                cookies: Some("sid=abc123".into()),
                checksum: None,
                queue: None,
            }],
        }
    );
}

#[test]
fn download_without_a_gui_fails_quickly_when_the_gui_cannot_start() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("no-such-vdm-gui.exe");
    let mut host = Host::start(dir.path(), Some(&missing));

    let started = Instant::now();
    let reply = host.request(&download_message());
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    assert_eq!(reply["type"], "result");
    assert_eq!(reply["ok"], false);
    assert!(
        reply["error"].as_str().is_some_and(|e| e.contains("no-such-vdm-gui")),
        "error should name the GUI path: {reply}"
    );
    assert!(host.finish().success());
}

#[test]
fn garbage_does_not_stop_the_host() {
    let dir = TempDir::new().unwrap();
    let mut host = Host::start(dir.path(), None);

    host.send(&json!({"type": "unknown"}));
    assert_eq!(host.recv()["ok"], false);

    host.send(&json!({"type": "download"}));
    assert_eq!(host.recv()["ok"], false);

    host.send(&json!([1, 2, 3]));
    assert_eq!(host.recv()["ok"], false);

    host.send_raw(b"{this is not json");
    assert_eq!(host.recv()["ok"], false);

    assert_eq!(host.request(&json!({"type": "ping"}))["type"], "pong");
    assert!(host.finish().success());
}
