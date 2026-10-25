//! Put the computer to sleep, hibernate it or shut it down, for a queue's
//! "when done" action.

use vdm_proto::WhenDone;

/// The command for `action`, as (program, arguments). `None` for actions the
/// app handles itself (nothing, closing Vibe) or this system can't do.
pub fn command_for(action: WhenDone) -> Option<(&'static str, Vec<&'static str>)> {
    if cfg!(windows) {
        Some(match action {
            WhenDone::ShutDown => ("shutdown", vec!["/s", "/t", "0"]),
            WhenDone::Hibernate => ("shutdown", vec!["/h"]),
            // `rundll32 powrprof.dll,SetSuspendState` hibernates when hibernation
            // is on; this asks for sleep explicitly.
            WhenDone::Sleep => (
                "powershell",
                vec![
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Add-Type -AssemblyName System.Windows.Forms; \
                     [System.Windows.Forms.Application]::SetSuspendState('Suspend', $false, $false)",
                ],
            ),
            WhenDone::Nothing | WhenDone::Exit => return None,
        })
    } else if cfg!(target_os = "macos") {
        Some(match action {
            WhenDone::ShutDown => {
                ("osascript", vec!["-e", "tell application \"System Events\" to shut down"])
            }
            // macOS decides between sleep and hibernation itself.
            WhenDone::Sleep | WhenDone::Hibernate => ("pmset", vec!["sleepnow"]),
            WhenDone::Nothing | WhenDone::Exit => return None,
        })
    } else {
        Some(match action {
            WhenDone::ShutDown => ("systemctl", vec!["poweroff"]),
            WhenDone::Sleep => ("systemctl", vec!["suspend"]),
            WhenDone::Hibernate => ("systemctl", vec!["hibernate"]),
            WhenDone::Nothing | WhenDone::Exit => return None,
        })
    }
}

/// Start `action`. Returns once the system has accepted the request.
pub fn run(action: WhenDone) -> Result<(), String> {
    let Some((program, args)) = command_for(action) else { return Ok(()) };
    let mut cmd = std::process::Command::new(program);
    cmd.args(&args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let status = cmd.status().map_err(|e| format!("{program}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{program} exited with {status}")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_system_action_has_a_command() {
        for action in [WhenDone::Sleep, WhenDone::Hibernate, WhenDone::ShutDown] {
            assert!(command_for(action).is_some(), "{action:?}");
        }
        assert!(command_for(WhenDone::Nothing).is_none());
        assert!(command_for(WhenDone::Exit).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_shutdown_is_immediate() {
        assert_eq!(command_for(WhenDone::ShutDown), Some(("shutdown", vec!["/s", "/t", "0"])));
    }
}
