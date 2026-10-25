//! Local IPC between VDM processes.
//!
//! The running GUI owns a local socket per data folder. Later launches of the
//! GUI, and the browser's native messaging host (`vdm-nmhost`), send it
//! [`Request`]s: URLs to add, or full downloads with cookies and referrer.
//!
//! Wire protocol: the client connects, writes one line of JSON (a [`Request`])
//! terminated by `\n`, and reads one line back. The server replies `ok\n` once
//! the request has parsed, then runs the callback. Anything else is a failed
//! delivery, and the client does not count it as handed off.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use interprocess::local_socket::{
    GenericNamespaced, Listener, ListenerOptions, Stream, ToNsName, prelude::*,
};
use serde::{Deserialize, Serialize};
use vdm_proto::NewDownload;

/// Longest line either side accepts, in bytes.
const MAX_LINE: usize = 1024 * 1024;
/// How long a read or write on the socket may wait before it fails.
const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// Pause after a failed accept, so a persistent error does not spin.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// What a later launch asks the running instance to do.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Request {
    pub urls: Vec<String>,
    /// Bring the window to the front.
    pub show: bool,
    /// Downloads with request details (cookies, referrer, suggested name),
    /// e.g. captured from the browser.
    #[serde(default)]
    pub downloads: Vec<NewDownload>,
}

/// VDM's standard per-user folders ("VDM" under the OS config/data roots).
pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "VDM")
}

/// The database of a data folder (`--data-dir`), or the standard one.
pub fn database_path(data_dir: Option<&Path>) -> Option<PathBuf> {
    match data_dir {
        Some(dir) => Some(dir.join("vdm.db")),
        None => project_dirs().map(|dirs| dirs.data_dir().join("vdm.db")),
    }
}

/// The socket of the GUI that uses `database`.
pub fn socket_for(database: &Path) -> String {
    socket_name(&database.to_string_lossy())
}

/// A local-socket name unique to one data folder, so a `--data-dir` test
/// instance never talks to the user's real one. Format: "vdm-<16 lowercase hex>.sock",
/// where the hex is a 64-bit FNV-1a hash of `key`.
pub fn socket_name(key: &str) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let hash =
        key.bytes().fold(OFFSET_BASIS, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(PRIME));
    format!("vdm-{hash:016x}.sock")
}

/// The result of [`acquire`].
#[derive(Debug)]
pub enum Instance {
    /// No other instance is running; this process now owns the socket.
    Primary(Server),
    /// Another instance is running and has received the request.
    Secondary,
}

/// Become the running instance, or deliver `request` to the one already running.
///
/// Tries to deliver first. If nobody answers, creates the listener. If that
/// fails, tries delivering once more (another instance may have just started),
/// then reclaims the name and creates the listener again.
pub fn acquire(name: &str, request: &Request) -> io::Result<Instance> {
    if deliver(name, request).is_ok() {
        return Ok(Instance::Secondary);
    }
    match create(name, false) {
        Ok(listener) => return Ok(Instance::Primary(Server { listener })),
        Err(err) => tracing::debug!(%err, "instance socket is not free"),
    }
    if deliver(name, request).is_ok() {
        return Ok(Instance::Secondary);
    }
    tracing::warn!("reclaiming the instance socket from a stale or unresponsive owner");
    let listener = create(name, true)?;
    Ok(Instance::Primary(Server { listener }))
}

/// The listening side, owned by the primary instance.
#[derive(Debug)]
pub struct Server {
    listener: Listener,
}

impl Server {
    /// Handle requests from later launches on a background thread, calling
    /// `on_request` for each one. Returns immediately. The thread runs until
    /// the process exits. A connection that sends garbage or disconnects early
    /// is logged with `tracing::warn!` and ignored; it must not stop the server.
    ///
    /// Each connection gets its own thread, so a client that connects and never
    /// sends a line cannot block later launches. Named pipes on Windows do not
    /// support timeouts, so on that platform only a thread per connection
    /// keeps such a client from stalling the others.
    pub fn serve(self, on_request: impl Fn(Request) + Send + 'static) {
        let on_request = Arc::new(Mutex::new(on_request));
        let listener = self.listener;
        let spawned = thread::Builder::new().name("vdm-ipc-accept".into()).spawn(move || {
            for accepted in listener.incoming() {
                match accepted {
                    Ok(stream) => spawn_connection(stream, Arc::clone(&on_request)),
                    Err(err) => {
                        tracing::warn!(%err, "cannot accept an instance connection");
                        thread::sleep(ACCEPT_RETRY_DELAY);
                    }
                }
            }
        });
        if let Err(err) = spawned {
            tracing::error!(%err, "cannot start the instance socket thread");
        }
    }
}

/// Create the listener for `name`. With `reclaim`, a name left in use by a
/// stale or unresponsive listener is taken over (Unix only; Windows ignores it).
fn create(name: &str, reclaim: bool) -> io::Result<Listener> {
    let name = name.to_ns_name::<GenericNamespaced>()?;
    ListenerOptions::new().name(name).try_overwrite(reclaim).create_sync()
}

/// Send `request` to the running instance and wait for its `ok` reply.
/// Send `request` to the running instance and wait for its `ok`.
pub fn deliver(name: &str, request: &Request) -> io::Result<()> {
    let name = name.to_ns_name::<GenericNamespaced>()?;
    let stream = Stream::connect(name)?;
    set_io_timeouts(&stream);

    let mut line = serde_json::to_string(request).map_err(io::Error::other)?;
    line.push('\n');
    let mut reader = BufReader::new(stream);
    reader.get_mut().write_all(line.as_bytes())?;
    reader.get_mut().flush()?;

    let reply = read_capped_line(&mut reader)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::UnexpectedEof, "no reply from the running instance")
    })?;
    if reply == b"ok" {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected reply from the running instance",
        ))
    }
}

/// Apply [`IO_TIMEOUT`] to both directions. Windows named pipes refuse
/// timeouts, so there the read can block until the peer answers or closes.
fn set_io_timeouts(stream: &Stream) {
    let recv = stream.set_recv_timeout(Some(IO_TIMEOUT));
    let send = stream.set_send_timeout(Some(IO_TIMEOUT));
    if let Err(err) = recv.and(send) {
        tracing::debug!(%err, "cannot set timeouts on the instance socket");
    }
}

/// Hand one accepted connection to its own thread.
fn spawn_connection<F>(stream: Stream, on_request: Arc<Mutex<F>>)
where
    F: Fn(Request) + Send + 'static,
{
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let spawned = thread::Builder::new()
        .name(format!("vdm-ipc-conn-{id}"))
        .spawn(move || handle_connection(id, stream, &on_request));
    if let Err(err) = spawned {
        tracing::warn!(conn = id, %err, "cannot start a thread for an instance connection");
    }
}

/// Read one request, reply `ok`, then run the callback. Bad input is logged
/// and dropped; the content is not logged, since it may hold anything.
fn handle_connection<F>(id: u64, stream: Stream, on_request: &Mutex<F>)
where
    F: Fn(Request),
{
    set_io_timeouts(&stream);
    let mut reader = BufReader::new(stream);
    let request = match read_request(&mut reader) {
        Ok(request) => request,
        Err(err) => {
            tracing::warn!(conn = id, %err, "ignoring a bad instance connection");
            return;
        }
    };
    if let Err(err) = reader.get_mut().write_all(b"ok\n").and_then(|()| reader.get_mut().flush()) {
        tracing::warn!(conn = id, %err, "cannot reply to an instance request");
        return;
    }
    let callback = on_request.lock().unwrap_or_else(PoisonError::into_inner);
    (*callback)(request);
}

/// Read one line and parse it as a [`Request`].
fn read_request(reader: &mut impl BufRead) -> io::Result<Request> {
    let line = read_capped_line(reader)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed before a request")
    })?;
    serde_json::from_slice(&line).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Read one `\n`-terminated line, without the terminator. `Ok(None)` means the
/// peer closed before sending anything. A line longer than [`MAX_LINE`] is an error.
fn read_capped_line<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed mid-line"))
            };
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let end = newline.unwrap_or(available.len());
        line.extend_from_slice(&available[..end]);
        reader.consume(newline.map_or(end, |index| index + 1));
        if line.len() > MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    /// A socket name that no other test, or other run of this test, uses.
    fn test_name(test: &str) -> String {
        socket_name(&format!("test-{}-{test}", std::process::id()))
    }

    /// Open a raw client connection to `name`, without the protocol.
    fn connect_raw(name: &str) -> Stream {
        let name = name.to_ns_name::<GenericNamespaced>().expect("valid name");
        Stream::connect(name).expect("connect")
    }

    /// Become the primary instance for `name` and serve it. Returns the requests it receives.
    fn serve_primary(name: &str) -> mpsc::Receiver<Request> {
        let Instance::Primary(server) = acquire(name, &Request::default()).expect("acquire") else {
            panic!("expected to become the primary instance");
        };
        let (tx, rx) = mpsc::channel();
        server.serve(move |request| {
            let _ = tx.send(request);
        });
        rx
    }

    fn sample(i: usize) -> Request {
        let downloads = if i.is_multiple_of(2) {
            vec![NewDownload {
                url: format!("https://example.com/{i}.iso"),
                save_dir: None,
                file_name: Some(format!("{i}.iso")),
                connections: None,
                referrer: Some("https://example.com/".into()),
                user_agent: Some("Mozilla/5.0".into()),
                cookies: Some("session=abc".into()),
                checksum: None,
                queue: None,
            }]
        } else {
            Vec::new()
        };
        Request {
            urls: vec![format!("https://example.com/{i}.zip")],
            show: i.is_multiple_of(2),
            downloads,
        }
    }

    #[test]
    fn requests_without_downloads_still_parse() {
        let req: Request =
            serde_json::from_str(r#"{"urls":["https://x/a.zip"],"show":true}"#).unwrap();
        assert_eq!(req.urls, ["https://x/a.zip"]);
        assert!(req.downloads.is_empty());
    }

    #[test]
    fn database_and_socket_follow_the_data_folder() {
        let dir = Path::new("D:/somewhere/vdm-data");
        assert_eq!(database_path(Some(dir)).unwrap(), dir.join("vdm.db"));
        let custom = socket_for(&database_path(Some(dir)).unwrap());
        assert_ne!(custom, socket_for(Path::new("D:/elsewhere/vdm.db")));
        if let Some(default) = database_path(None) {
            assert!(default.ends_with("vdm.db"));
        }
    }

    #[test]
    fn socket_name_is_stable() {
        // FNV-1a 64 of "foobar" is 0x85944171f73967e8.
        assert_eq!(socket_name("foobar"), "vdm-85944171f73967e8.sock");
    }

    #[test]
    fn socket_name_differs_per_key() {
        assert_ne!(socket_name("data-a"), socket_name("data-b"));
        assert_eq!(socket_name("data-a"), socket_name("data-a"));
    }

    #[test]
    fn socket_name_has_documented_format() {
        let name = socket_name("anything");
        let hex = name
            .strip_prefix("vdm-")
            .and_then(|rest| rest.strip_suffix(".sock"))
            .expect("vdm- prefix and .sock suffix");
        assert_eq!(hex.len(), 16);
        assert!(hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }

    #[test]
    fn first_acquire_is_primary_and_second_is_handed_off() {
        let name = test_name("handoff");
        let rx = serve_primary(&name);
        let request = sample(1);
        let instance = acquire(&name, &request).expect("acquire");
        assert!(matches!(instance, Instance::Secondary));
        assert_eq!(rx.recv_timeout(WAIT).expect("request delivered"), request);
    }

    #[test]
    fn sequential_secondaries_are_all_delivered_in_order() {
        let name = test_name("sequential");
        let rx = serve_primary(&name);
        for i in 0..5 {
            let request = sample(i);
            let instance = acquire(&name, &request).expect("acquire");
            assert!(matches!(instance, Instance::Secondary), "launch {i} should hand off");
            assert_eq!(rx.recv_timeout(WAIT).expect("request delivered"), request);
        }
    }

    #[test]
    fn garbage_connection_does_not_stop_the_server() {
        let name = test_name("garbage");
        let rx = serve_primary(&name);
        let mut bad = connect_raw(&name);
        bad.write_all(b"not json\n").expect("write garbage");
        drop(bad);

        let request = sample(2);
        let instance = acquire(&name, &request).expect("acquire");
        assert!(matches!(instance, Instance::Secondary));
        assert_eq!(rx.recv_timeout(WAIT).expect("request delivered"), request);
    }

    #[test]
    fn silent_connection_does_not_block_a_handoff() {
        let name = test_name("silent");
        let rx = serve_primary(&name);
        // Connected, but never sends a line.
        let _silent = connect_raw(&name);

        let request = sample(3);
        let instance = acquire(&name, &request).expect("acquire");
        assert!(matches!(instance, Instance::Secondary));
        assert_eq!(rx.recv_timeout(WAIT).expect("request delivered"), request);
    }

    #[test]
    fn name_is_reclaimed_after_the_primary_is_dropped() {
        let name = test_name("reclaim");
        let Instance::Primary(server) = acquire(&name, &Request::default()).expect("acquire")
        else {
            panic!("expected to become the primary instance");
        };
        // Dropped without serving, as if the owner went away.
        drop(server);

        let instance = acquire(&name, &Request::default()).expect("acquire again");
        assert!(matches!(instance, Instance::Primary(_)), "should become primary again");
    }
}
