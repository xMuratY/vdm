//! Run the test server by hand, e.g. to point a browser or `vdm-cli` at it.
//!
//! ```text
//! cargo run -p vdm-testsrv -- --size 104857600 --throttle-bps 2000000
//! ```

use std::net::SocketAddr;

use clap::Parser;
use vdm_testsrv::{Faults, TestFile, TestServer, pattern_bytes};

#[derive(Debug, Parser)]
#[command(about = "HTTP test server with Range support and fault injection")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Path the file is served at.
    #[arg(long, default_value = "/file.bin")]
    path: String,
    /// Size of the file in bytes.
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    size: usize,
    /// Ignore Range headers and omit Accept-Ranges.
    #[arg(long)]
    no_ranges: bool,
    /// Abort each response after this many bytes.
    #[arg(long)]
    cut_after: Option<u64>,
    /// With --cut-after, abort only every N-th GET response (N >= 1).
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    cut_every: Option<u64>,
    /// Answer the first N requests with 503.
    #[arg(long, default_value_t = 0)]
    fail_first: u64,
    /// Per-response speed limit in bytes per second.
    #[arg(long)]
    throttle_bps: Option<u64>,
    /// Omit Content-Length.
    #[arg(long)]
    hide_length: bool,
    /// Answer 503 to a GET that would exceed K concurrent bodies.
    #[arg(long)]
    max_concurrent: Option<usize>,
    /// Print each request's Range, Cookie, Referer and User-Agent.
    #[arg(long)]
    log_requests: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let faults = Faults {
        no_ranges: args.no_ranges,
        cut_after: args.cut_after,
        cut_every: args.cut_every,
        fail_first: args.fail_first,
        throttle_bps: args.throttle_bps,
        hide_length: args.hide_length,
        max_concurrent: args.max_concurrent,
    };
    let name = args.path.rsplit('/').next().unwrap_or("file.bin").to_owned();
    let file = TestFile::new(pattern_bytes(args.size, 1))
        .with_content_disposition(format!("attachment; filename=\"{}\"", name));
    let srv = TestServer::start_on(args.listen, [(args.path.clone(), file)], faults).await?;
    println!("serving {} ({} bytes); Ctrl+C to stop", srv.url(&args.path), args.size);
    if !args.log_requests {
        tokio::signal::ctrl_c().await?;
        return Ok(());
    }
    let mut seen = 0;
    loop {
        tokio::select! {
            r = tokio::signal::ctrl_c() => return Ok(r?),
            () = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
        }
        let ranges = srv.range_headers();
        let cookies = srv.header_values("cookie");
        let referers = srv.header_values("referer");
        let agents = srv.header_values("user-agent");
        for i in seen..ranges.len().min(cookies.len()) {
            println!(
                "request {i}: range={:?} cookie={:?} referer={:?} user-agent={:?}",
                ranges[i], cookies[i], referers[i], agents[i]
            );
        }
        seen = ranges.len().min(cookies.len());
    }
}
