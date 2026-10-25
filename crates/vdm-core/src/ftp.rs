//! FTP downloads on top of suppaftp's tokio client: [`probe`] reports size,
//! resumability and a file name; [`FtpDownload`] streams a file from an offset.
//!
//! Every network step is bounded by the caller's timeout. `ftps://` is not
//! supported yet: verifying the server needs a root-certificate store, and the
//! crate does not depend on one.

use std::future::Future;
use std::time::Duration;

use percent_encoding::percent_decode_str;
use reqwest::Url;
use suppaftp::tokio::{AsyncFtpStream, AsyncNoTlsStream, TransferStream};
use suppaftp::types::FileType;
use suppaftp::{FtpError, FtpResult, Status};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::error::{Error, Result};
use crate::filename;

/// Username and password; None means anonymous (user "anonymous", password "vdm@").
pub(crate) type Login<'a> = Option<(&'a str, &'a str)>;

const DEFAULT_PORT: u16 = 21;
const ANONYMOUS_USER: &str = "anonymous";
const ANONYMOUS_PASSWORD: &str = "vdm@";

/// Replies that mean "try again later" rather than "this will never work".
const TRANSIENT_CODES: [u32; 6] = [421, 425, 426, 450, 451, 452];

/// Replies to SIZE that mean the server does not implement it.
const SIZE_UNSUPPORTED_CODES: [u32; 4] = [500, 501, 502, 504];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FtpProbe {
    pub size: Option<u64>,
    pub resumable: bool,
    pub file_name: String,
}

/// Connect, log in, binary mode, SIZE, REST check, quit.
pub(crate) async fn probe(url: &Url, login: Login<'_>, timeout: Duration) -> Result<FtpProbe> {
    let target = Target::parse(url, login)?;
    let file_name = filename::choose(None, url);
    let mut ftp = start_session(&target, timeout).await?;
    let outcome = probe_session(&mut ftp, &target.path, timeout).await;
    quit_quietly(&mut ftp, timeout).await;
    let (size, resumable) = outcome?;
    Ok(FtpProbe { size, resumable, file_name })
}

async fn probe_session(
    ftp: &mut AsyncFtpStream,
    path: &str,
    timeout: Duration,
) -> Result<(Option<u64>, bool)> {
    let size = match timed(timeout, "SIZE", ftp.size(path)).await? {
        Ok(size) => Some(size as u64),
        Err(FtpError::UnexpectedResponse(reply))
            if SIZE_UNSUPPORTED_CODES.contains(&reply.status.code()) =>
        {
            None
        }
        Err(err) => return Err(ftp_error(err)),
    };
    let resumable = match timed(
        timeout,
        "REST",
        ftp.custom_command("REST 0", &[Status::RequestFilePending, Status::CommandOk]),
    )
    .await?
    {
        Ok(_) => true,
        Err(FtpError::UnexpectedResponse(reply)) if !is_transient(reply.status.code()) => false,
        Err(err) => return Err(ftp_error(err)),
    };
    Ok((size, resumable))
}

/// An open download: the control connection plus the data stream, positioned at `offset`.
pub(crate) struct FtpDownload {
    ftp: AsyncFtpStream,
    data: TransferStream<AsyncNoTlsStream>,
    timeout: Duration,
}

impl FtpDownload {
    /// Connect, log in, binary mode, REST `offset` (when > 0), RETR. Errors if offset > 0 and REST fails.
    ///
    /// Some servers only report a missing file after the data stream has opened, so
    /// the error can also surface from [`FtpDownload::read`] or [`FtpDownload::finish`].
    pub(crate) async fn open(
        url: &Url,
        login: Login<'_>,
        offset: u64,
        timeout: Duration,
    ) -> Result<Self> {
        let target = Target::parse(url, login)?;
        let mut ftp = start_session(&target, timeout).await?;
        if offset > 0 {
            let offset = usize::try_from(offset)
                .map_err(|_| invalid("download offset does not fit in usize"))?;
            // Only records the offset; the server is asked when RETR is sent.
            ftp.resume_transfer(offset).await.map_err(ftp_error)?;
        }
        let data =
            timed(timeout, "RETR", ftp.retr_as_stream(&target.path)).await?.map_err(ftp_error)?;
        tracing::debug!(host = %target.host, port = target.port, offset, "FTP transfer started");
        Ok(Self { ftp, data, timeout })
    }

    /// Read the next chunk into `buf`; Ok(0) at end of file.
    pub(crate) async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match tokio::time::timeout(self.timeout, self.data.read(buf)).await {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(err)) => {
                Err(Error::Ftp { message: format!("data connection: {err}"), transient: true })
            }
            Err(_) => Err(timeout_error("data read", self.timeout)),
        }
    }

    /// After Ok(0): close the data stream, read the transfer-complete reply, QUIT.
    pub(crate) async fn finish(self) -> Result<()> {
        let Self { mut ftp, data, timeout } = self;
        timed(timeout, "transfer completion", data.finish()).await?.map_err(ftp_error)?;
        // The transfer is complete at this point, so a failed QUIT does not change the result.
        quit_quietly(&mut ftp, timeout).await;
        Ok(())
    }
}

/// Where and how to connect, with everything decoded and checked.
#[derive(Debug)]
struct Target {
    /// Host without IPv6 brackets.
    host: String,
    port: u16,
    /// Percent-decoded absolute path, as sent to SIZE and RETR.
    path: String,
    user: String,
    password: String,
}

impl Target {
    fn parse(url: &Url, login: Login<'_>) -> Result<Self> {
        match url.scheme() {
            "ftp" => {}
            "ftps" => return Err(invalid("FTPS is not supported yet")),
            other => return Err(invalid(format!("unsupported scheme `{other}`"))),
        }
        let host = url
            .host_str()
            .map(|h| h.trim_start_matches('[').trim_end_matches(']'))
            .filter(|h| !h.is_empty())
            .ok_or_else(|| invalid("URL has no host"))?
            .to_owned();
        let path = decode(url.path())?;
        if path.is_empty() || path == "/" {
            return Err(invalid("URL has no file path"));
        }
        reject_control_chars(&path, "path")?;

        let (user, password) = match login {
            Some((user, password)) => (user.to_owned(), password.to_owned()),
            None if !url.username().is_empty() => {
                (decode(url.username())?, decode(url.password().unwrap_or(""))?)
            }
            None => (ANONYMOUS_USER.to_owned(), ANONYMOUS_PASSWORD.to_owned()),
        };
        // Each of these goes into a single control-channel line.
        reject_control_chars(&user, "user name")?;
        reject_control_chars(&password, "password")?;

        Ok(Self { host, port: url.port().unwrap_or(DEFAULT_PORT), path, user, password })
    }
}

/// Connect, log in and switch to binary mode.
async fn start_session(target: &Target, timeout: Duration) -> Result<AsyncFtpStream> {
    let tcp = timed(timeout, "connect", async {
        TcpStream::connect((target.host.as_str(), target.port))
            .await
            .map_err(FtpError::ConnectionError)
    })
    .await?
    .map_err(ftp_error)?;
    let mut ftp = timed(timeout, "greeting", AsyncFtpStream::connect_with_stream(tcp))
        .await?
        .map_err(ftp_error)?;
    timed(timeout, "login", ftp.login(&target.user, &target.password)).await?.map_err(ftp_error)?;
    timed(timeout, "TYPE I", ftp.transfer_type(FileType::Binary)).await?.map_err(ftp_error)?;
    Ok(ftp)
}

/// Best-effort QUIT: errors are logged and otherwise ignored.
async fn quit_quietly(ftp: &mut AsyncFtpStream, timeout: Duration) {
    let result = timed(timeout, "QUIT", ftp.quit()).await.and_then(|r| r.map_err(ftp_error));
    if let Err(err) = result {
        tracing::debug!("FTP QUIT failed: {err}");
    }
}

/// Run one network step under `timeout`. The outer error is only a timeout; the
/// inner result is the step's own FTP result.
async fn timed<T>(
    timeout: Duration,
    what: &str,
    step: impl Future<Output = FtpResult<T>>,
) -> Result<FtpResult<T>> {
    tokio::time::timeout(timeout, step).await.map_err(|_| timeout_error(what, timeout))
}

fn timeout_error(what: &str, timeout: Duration) -> Error {
    Error::Ftp { message: format!("{what} timed out after {timeout:?}"), transient: true }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Ftp { message: message.into(), transient: false }
}

fn is_transient(code: u32) -> bool {
    TRANSIENT_CODES.contains(&code)
}

/// Map a suppaftp error to ours. Connection and I/O errors are transient, as are
/// the 4xx replies in [`TRANSIENT_CODES`]; everything else is permanent.
fn ftp_error(err: FtpError) -> Error {
    match err {
        FtpError::UnexpectedResponse(reply) => {
            let code = reply.status.code();
            let text = reply.as_string().unwrap_or_default();
            let text = text.trim();
            // Status::Unknown carries no code (0), so only the text is shown then.
            let message = if code == 0 { text.to_owned() } else { format!("{code} {text}") };
            Error::Ftp { message, transient: is_transient(code) }
        }
        FtpError::ConnectionError(err) => {
            Error::Ftp { message: format!("connection: {err}"), transient: true }
        }
        other => Error::Ftp { message: other.to_string(), transient: false },
    }
}

fn decode(text: &str) -> Result<String> {
    percent_decode_str(text)
        .decode_utf8()
        .map(|s| s.into_owned())
        .map_err(|_| invalid("percent-encoded value is not valid UTF-8"))
}

fn reject_control_chars(value: &str, what: &str) -> Result<()> {
    if value.contains(['\r', '\n', '\0']) {
        return Err(invalid(format!("{what} contains a control character")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use libunftp::ServerBuilder;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use unftp_sbe_fs::Filesystem;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(10);

    /// Deterministic, non-repeating bytes, so shifted or truncated data is noticed.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[2]).collect()
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|l| l.local_addr())
            .expect("bind a free port")
            .port()
    }

    /// Anonymous libunftp server over `root`, on a fresh port.
    async fn serve(root: &Path) -> u16 {
        let port = free_port();
        let root = root.to_owned();
        let server = ServerBuilder::new(Box::new(move || {
            Filesystem::new(root.clone()).expect("open test root")
        }))
        .build()
        .expect("build libunftp server");
        tokio::spawn(async move {
            let _ = server.listen(format!("127.0.0.1:{port}")).await;
        });
        for _ in 0..500 {
            if TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.is_ok() {
                return port;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("test server did not start on port {port}");
    }

    fn url(port: u16, path: &str) -> Url {
        Url::parse(&format!("ftp://127.0.0.1:{port}{path}")).expect("test URL")
    }

    fn with_userinfo(port: u16, userinfo: &str, path: &str) -> Url {
        Url::parse(&format!("ftp://{userinfo}@127.0.0.1:{port}{path}")).expect("test URL")
    }

    /// Read a whole file through `FtpDownload`, using `buf_size`-byte reads.
    async fn download(
        url: &Url,
        login: Login<'_>,
        offset: u64,
        buf_size: usize,
    ) -> Result<Vec<u8>> {
        let mut dl = FtpDownload::open(url, login, offset, TIMEOUT).await?;
        let mut out = Vec::new();
        let mut buf = vec![0u8; buf_size];
        loop {
            let n = dl.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        dl.finish().await?;
        Ok(out)
    }

    fn permanent(err: &Error) -> bool {
        matches!(err, Error::Ftp { transient: false, .. })
    }

    fn transient(err: &Error) -> bool {
        matches!(err, Error::Ftp { transient: true, .. })
    }

    /// A control-channel server that accepts one login and records every line the
    /// client sends. After a successful login, RETR gets `550`, so a test can tell
    /// that the login passed by the error it sees.
    struct MockFtp {
        port: u16,
        seen: Arc<Mutex<Vec<String>>>,
    }

    async fn mock_ftp(user: &'static str, password: &'static str) -> MockFtp {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind mock");
        let port = listener.local_addr().expect("mock address").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                tokio::spawn(serve_mock(socket, user, password, log));
            }
        });
        MockFtp { port, seen }
    }

    async fn serve_mock(
        socket: TcpStream,
        user: &'static str,
        password: &'static str,
        log: Arc<Mutex<Vec<String>>>,
    ) {
        let (read, mut write) = socket.into_split();
        let mut lines = BufReader::new(read).lines();
        let _ = write.write_all(b"220 mock ready\r\n").await;
        let mut user_ok = false;
        // Keeps the passive listener alive until the client has connected to it.
        let mut passive: Option<TcpListener> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            log.lock().expect("log").push(line.clone());
            let (cmd, arg) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            let reply = match cmd.to_ascii_uppercase().as_str() {
                "USER" => {
                    user_ok = arg == user;
                    "331 Password required.".to_owned()
                }
                "PASS" if user_ok && arg == password => "230 Logged in.".to_owned(),
                "PASS" => "530 Login incorrect.".to_owned(),
                "TYPE" => "200 Type set.".to_owned(),
                "PASV" => {
                    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                        .await
                        .expect("bind passive listener");
                    let port = listener.local_addr().expect("passive address").port();
                    passive = Some(listener);
                    format!("227 Entering Passive Mode (127,0,0,1,{},{}).", port >> 8, port & 0xff)
                }
                "RETR" => "550 No such file.".to_owned(),
                "QUIT" => {
                    let _ = write.write_all(b"221 Bye.\r\n").await;
                    break;
                }
                _ => "502 Not implemented.".to_owned(),
            };
            if write.write_all(format!("{reply}\r\n").as_bytes()).await.is_err() {
                break;
            }
        }
        drop(passive);
    }

    #[tokio::test]
    async fn probe_reports_size_resumability_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let data = pattern(300 * 1024);
        std::fs::write(dir.path().join("data.bin"), &data).unwrap();
        let port = serve(dir.path()).await;

        let info = probe(&url(port, "/data.bin"), None, TIMEOUT).await.unwrap();
        assert_eq!(info.size, Some(data.len() as u64));
        assert!(info.resumable, "libunftp accepts REST");
        assert_eq!(info.file_name, "data.bin");
    }

    #[tokio::test]
    async fn probe_decodes_percent_encoded_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my file.bin"), pattern(1000)).unwrap();
        let port = serve(dir.path()).await;

        let info = probe(&url(port, "/my%20file.bin"), None, TIMEOUT).await.unwrap();
        assert_eq!(info.size, Some(1000));
        assert_eq!(info.file_name, "my file.bin");
    }

    #[tokio::test]
    async fn full_download_matches_file() {
        let dir = tempfile::tempdir().unwrap();
        let data = pattern(300 * 1024);
        std::fs::write(dir.path().join("data.bin"), &data).unwrap();
        let port = serve(dir.path()).await;

        let got = download(&url(port, "/data.bin"), None, 0, 64 * 1024).await.unwrap();
        assert!(got == data, "downloaded bytes differ from the file");
    }

    #[tokio::test]
    async fn download_from_offset_matches_tail() {
        let dir = tempfile::tempdir().unwrap();
        let data = pattern(300 * 1024);
        std::fs::write(dir.path().join("data.bin"), &data).unwrap();
        let port = serve(dir.path()).await;

        let offset = 123_457;
        let got = download(&url(port, "/data.bin"), None, offset, 16 * 1024).await.unwrap();
        assert!(got == data[offset as usize..], "tail differs from the file");
    }

    #[tokio::test]
    async fn small_reads_of_large_file_match() {
        let dir = tempfile::tempdir().unwrap();
        let data = pattern(600 * 1024);
        std::fs::write(dir.path().join("large.bin"), &data).unwrap();
        let port = serve(dir.path()).await;

        let got = download(&url(port, "/large.bin"), None, 0, 997).await.unwrap();
        assert!(got == data, "bytes read in 997-byte chunks differ from the file");
    }

    #[tokio::test]
    async fn anonymous_is_used_without_credentials() {
        let mock = mock_ftp(ANONYMOUS_USER, ANONYMOUS_PASSWORD).await;
        let err = FtpDownload::open(&url(mock.port, "/file.bin"), None, 0, TIMEOUT)
            .await
            .err()
            .expect("mock refuses RETR");
        assert!(err.to_string().contains("550"), "login should pass: {err}");
        let seen = mock.seen.lock().unwrap().clone();
        assert!(seen.contains(&"USER anonymous".to_owned()), "{seen:?}");
        assert!(seen.contains(&"PASS vdm@".to_owned()), "{seen:?}");
    }

    #[tokio::test]
    async fn login_argument_overrides_url_credentials() {
        let mock = mock_ftp("alice", "s3cret").await;
        let url = with_userinfo(mock.port, "bob:wrong", "/file.bin");
        let err = FtpDownload::open(&url, Some(("alice", "s3cret")), 0, TIMEOUT)
            .await
            .err()
            .expect("mock refuses RETR");
        assert!(err.to_string().contains("550"), "login should pass: {err}");
        let seen = mock.seen.lock().unwrap().clone();
        assert!(seen.contains(&"USER alice".to_owned()), "{seen:?}");
        assert!(seen.contains(&"PASS s3cret".to_owned()), "{seen:?}");
        assert!(!seen.iter().any(|l| l.contains("bob")), "{seen:?}");
    }

    #[tokio::test]
    async fn url_credentials_are_percent_decoded() {
        let mock = mock_ftp("bob", "p@ss").await;
        let url = with_userinfo(mock.port, "bob:p%40ss", "/file.bin");
        let err = FtpDownload::open(&url, None, 0, TIMEOUT).await.err().expect("mock refuses RETR");
        assert!(err.to_string().contains("550"), "login should pass: {err}");
        let seen = mock.seen.lock().unwrap().clone();
        assert!(seen.contains(&"PASS p@ss".to_owned()), "{seen:?}");
    }

    #[tokio::test]
    async fn wrong_password_is_permanent_with_reply_text() {
        let mock = mock_ftp("alice", "s3cret").await;
        let err =
            FtpDownload::open(&url(mock.port, "/file.bin"), Some(("alice", "nope")), 0, TIMEOUT)
                .await
                .err()
                .expect("login must fail");
        assert!(permanent(&err), "{err:?}");
        assert!(err.to_string().contains("530"), "{err}");
        assert!(err.to_string().contains("Login incorrect"), "{err}");
    }

    #[tokio::test]
    async fn missing_file_is_permanent() {
        let dir = tempfile::tempdir().unwrap();
        let port = serve(dir.path()).await;

        let err = probe(&url(port, "/missing.bin"), None, TIMEOUT).await.expect_err("probe");
        assert!(permanent(&err), "{err:?}");
        assert!(err.to_string().contains("550"), "{err}");

        // libunftp answers RETR with 150 before it looks at the file, so the 550 may only
        // arrive when the transfer is finished. Either way the download must fail.
        let err = download(&url(port, "/missing.bin"), None, 0, 1024).await.expect_err("download");
        assert!(permanent(&err), "{err:?}");
        assert!(err.to_string().contains("550"), "{err}");
    }

    #[tokio::test]
    async fn connection_refused_is_transient() {
        let port = free_port();
        let err = probe(&url(port, "/data.bin"), None, TIMEOUT).await.expect_err("probe");
        assert!(transient(&err), "{err:?}");
        let err =
            FtpDownload::open(&url(port, "/data.bin"), None, 0, TIMEOUT).await.err().expect("open");
        assert!(transient(&err), "{err:?}");
    }

    #[tokio::test]
    async fn ftps_is_not_supported_yet() {
        let url = Url::parse("ftps://127.0.0.1:990/data.bin").unwrap();
        let err = probe(&url, None, TIMEOUT).await.expect_err("probe");
        assert!(permanent(&err), "{err:?}");
        assert_eq!(err.to_string(), "FTP: FTPS is not supported yet");
    }
}
