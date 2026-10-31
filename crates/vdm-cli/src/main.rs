//! `vdm`: the command-line client for Vibe Download Manager.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use vdm_core::{
    CancellationToken, Checksum, DownloadRequest, Downloader, Error, Options, Progress,
    ProxySetting, Quality, RateLimiter, SiteLogin,
};

/// Exit code when the download was interrupted and can be resumed.
const EXIT_PAUSED: u8 = 130;

#[derive(Debug, Parser)]
#[command(name = "vdm", version, about = "Vibe Download Manager (command line)")]
struct Cli {
    /// More logging (-v info, -vv debug, -vvv trace). RUST_LOG overrides this.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Show size, range support, validators and file name for a URL.
    Probe {
        url: String,
        #[command(flatten)]
        http: HttpArgs,
    },
    /// Download a URL. Run the same command again to resume an interrupted download.
    Get {
        url: String,
        /// Folder to save into.
        #[arg(short = 'o', long, default_value = ".")]
        out_dir: PathBuf,
        /// File name override.
        #[arg(short = 'n', long)]
        name: Option<String>,
        /// Parallel connections. Servers that can't resume always get one.
        #[arg(
            short = 'c',
            long,
            default_value_t = 8,
            value_parser = clap::value_parser!(u8).range(1..=i64::from(vdm_core::MAX_CONNECTIONS)),
        )]
        connections: u8,
        /// Speed limit in bytes per second; K, M and G suffixes are binary (e.g. 500K, 2M).
        #[arg(long, value_parser = parse_rate)]
        limit: Option<u64>,
        /// Fail instead of starting over if the file changed on the server.
        #[arg(long)]
        no_restart: bool,
        /// Expected checksum, e.g. sha256:…; the file is verified after downloading.
        #[arg(long)]
        checksum: Option<Checksum>,
        /// For video streams (HLS/DASH): best, worst, or a maximum height like 720.
        #[arg(short = 'q', long, default_value = "best")]
        quality: Quality,
        #[command(flatten)]
        http: HttpArgs,
    },
}

#[derive(Debug, clap::Args)]
struct HttpArgs {
    /// Extra request header, e.g. -H "Cookie: a=b". Repeatable.
    #[arg(short = 'H', long = "header", value_parser = parse_header)]
    headers: Vec<(String, String)>,
    /// Retries after consecutive failures with no new data.
    #[arg(long, default_value_t = 5)]
    retries: u32,
    /// Proxy: "system" (default), "none", or a URL like socks5://127.0.0.1:1080.
    #[arg(long, default_value = "system")]
    proxy: ProxySetting,
    /// Username and password for the URL's site (HTTP Basic auth or FTP login).
    #[arg(long, value_name = "USER:PASSWORD", value_parser = parse_login)]
    login: Option<(String, String)>,
    /// Torrents: share with other peers while downloading (by default
    /// nothing is uploaded). `vdm` exits when done, so it never seeds.
    #[arg(long)]
    upload: bool,
}

fn parse_login(s: &str) -> Result<(String, String), String> {
    let (user, password) = s.split_once(':').ok_or("expected `user:password`")?;
    if user.is_empty() {
        return Err("the user name is empty".into());
    }
    Ok((user.to_owned(), password.to_owned()))
}

fn parse_header(s: &str) -> Result<(String, String), String> {
    let (name, value) = s.split_once(':').ok_or("expected `Name: value`")?;
    if name.trim().is_empty() {
        return Err("header name is empty".into());
    }
    Ok((name.trim().to_owned(), value.trim().to_owned()))
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let cancel = CancellationToken::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });

    match run(cli.command, cancel).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Cancelled { downloaded }) => {
            eprintln!(
                "\nPaused after {}. Run the same command again to resume.",
                fmt_bytes(downloaded)
            );
            ExitCode::from(EXIT_PAUSED)
        }
        Err(err) => {
            eprintln!("\nerror: {}", error_chain(&err));
            ExitCode::FAILURE
        }
    }
}

async fn run(cmd: Cmd, cancel: CancellationToken) -> vdm_core::Result<()> {
    match cmd {
        Cmd::Probe { url, http } => {
            let dl = downloader(&http, &url, true)?;
            if vdm_core::is_media_url(&url) {
                return print_media(&dl, &url, &http, &cancel).await;
            }
            let info = dl.probe(&url, &http.headers, &cancel).await?;
            if info.content_type.as_deref().is_some_and(vdm_core::is_playlist_type) {
                return print_media(&dl, &url, &http, &cancel).await;
            }
            let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
            println!("URL:           {url}");
            println!("Final URL:     {}", info.final_url);
            println!("File name:     {}", info.file_name);
            match info.size {
                Some(size) => println!("Size:          {} ({size} bytes)", fmt_bytes(size)),
                None => println!("Size:          unknown"),
            }
            println!("Resumable:     {}", if info.resumable { "yes" } else { "no" });
            println!("ETag:          {}", opt(&info.etag));
            println!("Last-Modified: {}", opt(&info.last_modified));
            println!("Content-Type:  {}", opt(&info.content_type));
            Ok(())
        }
        Cmd::Get {
            url,
            out_dir,
            name,
            connections,
            limit,
            no_restart,
            checksum,
            quality,
            http,
        } => {
            let dl = downloader(&http, &url, !no_restart)?;
            let req = DownloadRequest {
                url,
                out_dir,
                file_name: name,
                connections: Some(connections),
                headers: http.headers,
                limits: limit
                    .map(|l| vec![Arc::new(RateLimiter::new(Some(l)))])
                    .unwrap_or_default(),
                quality,
            };
            let mut meter = ProgressMeter::new(std::io::stderr().is_terminal());
            let done = dl.download(req, |p| meter.update(p), cancel).await?;
            meter.finish();
            let mut line = format!("Saved {} ({})", done.path.display(), fmt_bytes(done.size));
            if done.resumed_from > 0 {
                line += &format!(", resumed from {}", fmt_bytes(done.resumed_from));
            }
            if done.restarted {
                line += ", restarted because the old part could not be resumed";
            }
            println!("{line}");
            if let Some(expected) = checksum {
                let actual = vdm_core::checksum::hash_file(&done.path, expected.algorithm)
                    .await
                    .map_err(|source| Error::Io { path: done.path.clone(), source })?;
                if !actual.eq_ignore_ascii_case(&expected.hex) {
                    return Err(Error::Protocol(format!(
                        "checksum mismatch: expected {expected}, got {}:{actual} (the file was kept)",
                        expected.algorithm.name()
                    )));
                }
                println!("Checksum OK ({expected})");
            }
            Ok(())
        }
    }
}

/// `123`, `500K`, `1.5M`, `2G` (binary multiples) as bytes per second.
fn parse_rate(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let mult: u64 = match c.to_ascii_uppercase() {
                'K' => 1 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                _ => return Err(format!("unknown unit `{c}`; use K, M or G")),
            };
            (&s[..i], mult)
        }
        _ => (s, 1),
    };
    let value: f64 = num.trim().parse().map_err(|_| format!("`{s}` is not a speed"))?;
    if !(value.is_finite() && value > 0.0) {
        return Err("speed must be greater than zero".into());
    }
    Ok((value * mult as f64).round().max(1.0) as u64)
}

/// `vdm probe` for an HLS/DASH stream: its qualities and the default pick.
async fn print_media(
    dl: &Downloader,
    url: &str,
    http: &HttpArgs,
    cancel: &CancellationToken,
) -> vdm_core::Result<()> {
    let info = dl.probe_media(url, &http.headers, Quality::Best, cancel).await?;
    let kind = match info.kind {
        vdm_core::MediaKind::Hls => "HLS",
        vdm_core::MediaKind::Dash => "DASH",
    };
    println!("URL:       {url}");
    println!("Stream:    {kind}{}", if info.separate_audio { ", separate audio" } else { "" });
    if info.duration_ms > 0 {
        let s = info.duration_ms / 1000;
        println!("Duration:  {}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    }
    println!("Qualities:");
    for (i, v) in info.variants.iter().enumerate() {
        let mark = if info.chosen == Some(i) { "  <- default (best)" } else { "" };
        let codecs = v.codecs.as_deref().map(|c| format!("  [{c}]")).unwrap_or_default();
        println!("  {}{codecs}{mark}", v.label());
    }
    Ok(())
}

fn downloader(
    http: &HttpArgs,
    url: &str,
    restart_if_changed: bool,
) -> vdm_core::Result<Downloader> {
    let mut opts = Options {
        restart_if_changed,
        proxy: http.proxy.clone(),
        torrent_upload: http.upload,
        ..Options::default()
    };
    opts.retry.max_attempts = http.retries.saturating_add(1);
    let host = vdm_core::url_host(url);
    if let (Some((username, password)), Some(site)) = (&http.login, host) {
        opts.logins =
            vec![SiteLogin { site, username: username.clone(), password: password.clone() }];
    }
    Downloader::new(opts)
}

/// Single-line progress on stderr, redrawn at most 4 times a second.
struct ProgressMeter {
    enabled: bool,
    started: Instant,
    last_draw: Option<Instant>,
    /// Smoothed speed in bytes/s and the sample it was computed from.
    speed: f64,
    sample: Option<(Instant, u64)>,
    drew: bool,
}

impl ProgressMeter {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            started: Instant::now(),
            last_draw: None,
            speed: 0.0,
            sample: None,
            drew: false,
        }
    }

    fn update(&mut self, p: Progress) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if self.last_draw.is_some_and(|t| now - t < Duration::from_millis(250)) {
            return;
        }
        if let Some((t, bytes)) = self.sample {
            let dt = (now - t).as_secs_f64();
            if dt > 0.0 && p.downloaded >= bytes {
                let instant = (p.downloaded - bytes) as f64 / dt;
                self.speed =
                    if self.speed == 0.0 { instant } else { 0.7 * self.speed + 0.3 * instant };
            }
        }
        self.sample = Some((now, p.downloaded));
        self.last_draw = Some(now);

        let mut line = match p.total {
            Some(total) if total > 0 => format!(
                "{} / {}  {:5.1}%",
                fmt_bytes(p.downloaded),
                fmt_bytes(total),
                p.downloaded as f64 * 100.0 / total as f64
            ),
            _ => fmt_bytes(p.downloaded),
        };
        line += &format!("  {}/s", fmt_bytes(self.speed as u64));
        if let Some(total) = p.total
            && self.speed > 0.0
            && total > p.downloaded
        {
            let eta = ((total - p.downloaded) as f64 / self.speed) as u64;
            line += &format!("  ETA {}:{:02}:{:02}", eta / 3600, eta / 60 % 60, eta % 60);
        }
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{line:<60}");
        let _ = err.flush();
        self.drew = true;
    }

    fn finish(&self) {
        if self.drew {
            eprintln!("\r{:<60}\rDone in {:.1}s", "", self.started.elapsed().as_secs_f64());
        }
    }
}

fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[unit]) }
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut s = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        let msg = e.to_string();
        if !s.contains(&msg) {
            s += &format!(": {msg}");
        }
        source = e.source();
    }
    s
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn get_rejects_zero_connections() {
        assert!(Cli::try_parse_from(["vdm", "get", "http://x", "-c", "0"]).is_err());
        assert!(Cli::try_parse_from(["vdm", "get", "http://x", "-c", "16"]).is_ok());
    }

    #[test]
    fn header_args() {
        assert_eq!(parse_header("Cookie: a=b; c=d").unwrap(), ("Cookie".into(), "a=b; c=d".into()));
        assert!(parse_header("no-colon").is_err());
        assert!(parse_header(": value").is_err());
    }

    #[test]
    fn rate_args() {
        assert_eq!(parse_rate("1000").unwrap(), 1000);
        assert_eq!(parse_rate("500K").unwrap(), 500 * 1024);
        assert_eq!(parse_rate("1.5m").unwrap(), 1_572_864);
        assert_eq!(parse_rate("2G").unwrap(), 2 << 30);
        assert!(parse_rate("0").is_err());
        assert!(parse_rate("-5").is_err());
        assert!(parse_rate("5X").is_err());
        assert!(parse_rate("fast").is_err());
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(10 * 1024 * 1024), "10.0 MiB");
    }
}
