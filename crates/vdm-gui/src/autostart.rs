//! Start Vibe when the user signs in, minimized to the tray.
//!
//! Windows: a value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! (the installer writes the same value). The choice is also kept as
//! `HKCU\Software\Vibe Download Manager\AutoStart`, which the installer reads
//! so a silent update keeps it. Linux: an XDG autostart `.desktop` file.
//! macOS: a LaunchAgent.

/// The argument that makes the app start hidden in the tray.
pub const MINIMIZED_ARG: &str = "--minimized";

/// Whether the app starts at sign-in.
pub fn is_enabled() -> bool {
    imp::is_enabled()
}

/// Turn starting at sign-in on or off for the running executable.
pub fn set(enabled: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    imp::set(enabled, &exe)
}

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const APP: &str = r"Software\Vibe Download Manager";
    const NAME: &str = "Vibe Download Manager";

    pub fn is_enabled() -> bool {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN, KEY_READ)
            .and_then(|key| key.get_value::<String, _>(NAME))
            .is_ok()
    }

    pub fn set(enabled: bool, exe: &Path) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (run, _) = hkcu.create_subkey(RUN).map_err(|e| e.to_string())?;
        if enabled {
            let command = format!("\"{}\" {}", exe.display(), super::MINIMIZED_ARG);
            run.set_value(NAME, &command).map_err(|e| e.to_string())?;
        } else {
            match run.delete_value(NAME) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.to_string()),
            }
        }
        // Remembered for the installer, so updates keep the choice.
        if let Ok((app, _)) = hkcu.create_subkey_with_flags(APP, KEY_WRITE) {
            let _ = app.set_value("AutoStart", &(if enabled { "1" } else { "0" }));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::{Path, PathBuf};

    fn file() -> Option<PathBuf> {
        directories::BaseDirs::new()
            .map(|d| d.config_dir().join("autostart").join("vibe-download-manager.desktop"))
    }

    pub fn is_enabled() -> bool {
        file().is_some_and(|f| f.is_file())
    }

    pub fn set(enabled: bool, exe: &Path) -> Result<(), String> {
        let file = file().ok_or("no home folder")?;
        if !enabled {
            return match std::fs::remove_file(&file) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.to_string()),
                _ => Ok(()),
            };
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Vibe Download Manager\nExec=\"{}\" {}\nX-GNOME-Autostart-enabled=true\n",
            exe.display(),
            super::MINIMIZED_ARG
        );
        std::fs::write(&file, entry).map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::{Path, PathBuf};

    fn file() -> Option<PathBuf> {
        directories::BaseDirs::new()
            .map(|d| d.home_dir().join("Library/LaunchAgents/com.vdm.gui.plist"))
    }

    pub fn is_enabled() -> bool {
        file().is_some_and(|f| f.is_file())
    }

    pub fn set(enabled: bool, exe: &Path) -> Result<(), String> {
        let file = file().ok_or("no home folder")?;
        if !enabled {
            return match std::fs::remove_file(&file) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.to_string()),
                _ => Ok(()),
            };
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let exe = exe.display().to_string().replace('&', "&amp;").replace('<', "&lt;");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.vdm.gui</string>
<key>ProgramArguments</key><array><string>{exe}</string><string>{}</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>
"#,
            super::MINIMIZED_ARG
        );
        std::fs::write(&file, plist).map_err(|e| e.to_string())
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    pub fn is_enabled() -> bool {
        false
    }

    pub fn set(_enabled: bool, _exe: &std::path::Path) -> Result<(), String> {
        Err("starting at sign-in is not supported on this system".into())
    }
}
