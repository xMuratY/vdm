//! VDM's browser native-messaging host.
//!
//! The browser starts this program with the caller's origin as an argument
//! and talks to it over stdin/stdout. Downloads are handed to the running
//! VDM GUI, which is started if needed. `install` and `uninstall` register the
//! host with browsers instead.

mod forward;
mod framing;
mod host;
mod install;

use std::env;
use std::io;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "vdm-nmhost",
    version,
    about = "Vibe Download Manager browser native messaging host"
)]
struct Cli {
    #[command(subcommand)]
    command: Sub,
}

#[derive(Debug, Subcommand)]
enum Sub {
    /// Register the host with Chrome-family browsers and Firefox.
    Install {
        /// Print the changes without making them.
        #[arg(long)]
        dry_run: bool,
        /// Chrome-family extension id allowed to use the host (repeatable).
        /// Without this option, the VDM extension id is used.
        #[arg(long = "chrome-id", value_name = "ID")]
        chrome_ids: Vec<String>,
        /// Firefox extension id allowed to use the host (repeatable).
        /// Without this option, the VDM extension id is used.
        #[arg(long = "firefox-id", value_name = "ID")]
        firefox_ids: Vec<String>,
    },
    /// Remove the registrations made by `install`.
    Uninstall {
        /// Print the changes without making them.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> Result<()> {
    init_logging();
    match env::args_os().nth(1).and_then(|arg| arg.into_string().ok()) {
        Some(sub) if sub == "install" || sub == "uninstall" => run_subcommand(Cli::parse()),
        _ => run_host(),
    }
}

/// Logs go to stderr: stdout belongs to the browser while in host mode.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();
}

fn run_host() -> Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    host::serve(&mut input, &mut output, forward::deliver).context("native messaging stream failed")
}

fn run_subcommand(cli: Cli) -> Result<()> {
    let exe = env::current_exe().context("cannot find the host's own executable")?;
    let home = env::home_dir().context("cannot find the home folder")?;
    let os = install::Os::current();
    match cli.command {
        Sub::Install { dry_run, chrome_ids, firefox_ids } => {
            let chrome = default_ids(chrome_ids, install::DEFAULT_CHROME_ID);
            let firefox = default_ids(firefox_ids, install::DEFAULT_FIREFOX_ID);
            let actions = install::plan_install(&exe, os, &home, &chrome, &firefox);
            install::apply(&actions, dry_run)
        }
        Sub::Uninstall { dry_run } => {
            let actions = install::plan_uninstall(&exe, os, &home);
            install::apply(&actions, dry_run)
        }
    }
}

fn default_ids(ids: Vec<String>, default: &str) -> Vec<String> {
    if ids.is_empty() { vec![default.to_string()] } else { ids }
}
