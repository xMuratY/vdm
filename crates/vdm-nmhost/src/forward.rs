//! Hands downloads to the running VDM GUI, starting the GUI when none answers.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use vdm_ipc::Request;

/// How long to wait for a freshly started GUI to answer.
const STARTUP_WAIT: Duration = Duration::from_secs(10);
/// Pause between delivery attempts while the GUI starts.
const RETRY_EVERY: Duration = Duration::from_millis(200);

/// Deliver `request` to the GUI of the data folder in `VDM_DATA_DIR` (or the
/// standard one). If no GUI answers, start it and retry until it does.
pub fn deliver(request: &Request) -> Result<()> {
    let data_dir = env::var_os("VDM_DATA_DIR").filter(|v| !v.is_empty()).map(PathBuf::from);
    let database =
        vdm_ipc::database_path(data_dir.as_deref()).context("cannot find the VDM data folder")?;
    let socket = vdm_ipc::socket_for(&database);

    if vdm_ipc::deliver(&socket, request).is_ok() {
        return Ok(());
    }
    let gui = gui_path()?;
    tracing::info!("VDM is not running; starting it");
    let _gui = start(&gui, data_dir.as_deref())?;

    let deadline = Instant::now() + STARTUP_WAIT;
    loop {
        thread::sleep(RETRY_EVERY);
        match vdm_ipc::deliver(&socket, request) {
            Ok(()) => return Ok(()),
            Err(err) if Instant::now() < deadline => {
                tracing::debug!(%err, "VDM is not answering yet");
            }
            Err(err) => {
                bail!("VDM did not answer within {} seconds: {err}", STARTUP_WAIT.as_secs())
            }
        }
    }
}

/// The GUI executable: `VDM_GUI_PATH`, or `vdm-gui` next to this executable.
fn gui_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("VDM_GUI_PATH").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let exe = env::current_exe().context("cannot find the host's own executable")?;
    let dir = exe.parent().context("the host's executable has no folder")?;
    Ok(dir.join(format!("vdm-gui{}", env::consts::EXE_SUFFIX)))
}

/// Start the GUI detached, with no stdio, so it never writes to the browser's pipe.
fn start(gui: &Path, data_dir: Option<&Path>) -> Result<Child> {
    let mut command = Command::new(gui);
    if let Some(dir) = data_dir {
        command.arg("--data-dir").arg(dir);
    }
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Chrome runs native hosts in a job object that is closed (killing its
    // processes) when the host exits; break away from it so VDM keeps running.
    // If the job forbids that, start VDM inside it rather than not at all.
    detach(&mut command, true);
    match command.spawn() {
        Ok(child) => Ok(child),
        Err(err) if cfg!(windows) => {
            tracing::debug!(%err, "could not leave the browser's job; starting VDM inside it");
            detach(&mut command, false);
            command.spawn().with_context(|| format!("cannot start VDM from {}", gui.display()))
        }
        Err(err) => Err(err).with_context(|| format!("cannot start VDM from {}", gui.display())),
    }
}

#[cfg(unix)]
fn detach(command: &mut Command, _break_away: bool) {
    use std::os::unix::process::CommandExt;
    // Own process group, so the browser closing its group does not stop VDM.
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command, break_away: bool) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    if break_away {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    command.creation_flags(flags);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command, _break_away: bool) {}
