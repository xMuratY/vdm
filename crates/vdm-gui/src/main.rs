//! Vibe Download Manager: the desktop app.

// No console window for release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod categories;
mod columns;
mod details;
mod i18n;
mod icon;
mod icons;
mod logins;
mod model;
mod notify;
mod power;
mod queues;
mod settings;
mod theme;
mod tray;
mod update;

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use vdm_ipc as instance;

#[derive(Debug, Parser)]
#[command(name = "vdm-gui", version, about = "Vibe Download Manager")]
struct Args {
    /// URLs to add at startup.
    urls: Vec<String>,
    /// Keep settings and the database in this folder instead of the user profile.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Save a PNG of the window to this file and quit (for checking the layout).
    #[arg(long, hide = true)]
    screenshot: Option<PathBuf>,
    /// Open a dialog at startup: "add" or "settings".
    #[arg(long, hide = true)]
    open: Option<String>,
    /// Seconds to wait before taking the screenshot.
    #[arg(long, default_value_t = 2.0, hide = true)]
    screenshot_delay: f64,
    /// Start hidden in the tray (used when starting at sign-in).
    #[arg(long)]
    minimized: bool,
}

fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();

    let args = Args::parse();
    let paths = match &args.data_dir {
        Some(dir) => settings::Paths::in_dir(dir)?,
        None => settings::Paths::discover()?,
    };
    let settings = settings::Settings::load(&paths.settings)?;
    app::apply_language(&settings);

    // One instance per data folder: a second launch hands its URLs to the first.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    if args.screenshot.is_none() {
        let name = instance::socket_for(&paths.database);
        let request = instance::Request {
            urls: args.urls.clone(),
            show: !args.minimized,
            ..Default::default()
        };
        match instance::acquire(&name, &request) {
            Ok(instance::Instance::Secondary) => {
                println!(
                    "Vibe Download Manager is already running; sent it {} address(es).",
                    request.urls.len()
                );
                return Ok(());
            }
            Ok(instance::Instance::Primary(server)) => server.serve(move |req| {
                let _ = tx.send(req);
            }),
            Err(err) => tracing::warn!(%err, "running without single-instance support"),
        }
    }

    let screenshot =
        args.screenshot.map(|p| (p, Duration::from_secs_f64(args.screenshot_delay.max(0.0))));
    app::run(app::Config {
        paths,
        settings,
        add: args.urls,
        screenshot,
        open: args.open,
        inbox: app::Inbox::new(rx),
        start_minimized: args.minimized,
    })
}
