//! VDM release tasks. Run with `cargo xtask <task>`.

mod dist;
mod icons;
mod package;
mod site;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "VDM release tasks")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Draw the app icon and write assets/icons (ICO + PNGs).
    Icons,
    /// Build release binaries, the MSI installer and the update manifest.
    Dist(dist::Args),
    /// Build the website (GitHub Pages) from site/ into target/site.
    Site(site::Args),
    /// Build the Linux tarball or the macOS disk image (on that system).
    Package(package::Args),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().task {
        Task::Icons => icons::run(),
        Task::Dist(args) => dist::run(args),
        Task::Site(args) => site::run(args),
        Task::Package(args) => package::run(args),
    }
}
