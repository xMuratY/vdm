//! Registers the host with browsers (`vdm-nmhost install`) and removes that
//! registration again (`vdm-nmhost uninstall`).
//!
//! Planning is pure: [`plan_install`] and [`plan_uninstall`] return the
//! [`Action`]s to take, and [`apply`] carries them out, or only prints them
//! for a dry run.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::json;

pub const HOST_NAME: &str = "com.vdm.native";
pub const DEFAULT_CHROME_ID: &str = "gepekebdmobfjhpghgflnhgbbmjcmhon";
pub const DEFAULT_FIREFOX_ID: &str = "vdm@vdm.local";

const DESCRIPTION: &str = "Vibe Download Manager";
/// Manifest file name in each browser folder on Linux and macOS.
const MANIFEST_FILE: &str = "com.vdm.native.json";
/// Manifest file names next to the executable on Windows.
const WINDOWS_CHROME_MANIFEST: &str = "com.vdm.native.chrome.json";
const WINDOWS_FIREFOX_MANIFEST: &str = "com.vdm.native.firefox.json";

/// Chrome-family browsers' NativeMessagingHosts keys under HKCU.
const WINDOWS_CHROME_KEYS: [&str; 4] = [
    r"Software\Google\Chrome\NativeMessagingHosts",
    r"Software\Microsoft\Edge\NativeMessagingHosts",
    r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
    r"Software\Chromium\NativeMessagingHosts",
];
const WINDOWS_FIREFOX_KEY: &str = r"Software\Mozilla\NativeMessagingHosts";

/// Chrome-family folders on Linux, relative to the home folder.
const LINUX_CHROME_DIRS: [&str; 4] = [
    ".config/google-chrome/NativeMessagingHosts",
    ".config/chromium/NativeMessagingHosts",
    ".config/microsoft-edge/NativeMessagingHosts",
    ".config/BraveSoftware/Brave-Browser/NativeMessagingHosts",
];
const LINUX_FIREFOX_DIR: &str = ".mozilla/native-messaging-hosts";

/// Chrome-family folders on macOS, relative to the home folder.
const MACOS_CHROME_DIRS: [&str; 4] = [
    "Library/Application Support/Google/Chrome/NativeMessagingHosts",
    "Library/Application Support/Chromium/NativeMessagingHosts",
    "Library/Application Support/Microsoft Edge/NativeMessagingHosts",
    "Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts",
];
const MACOS_FIREFOX_DIR: &str = "Library/Application Support/Mozilla/NativeMessagingHosts";

/// The operating system whose registration rules apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(clippy::enum_variant_names, reason = "the variant names are the public API")]
pub enum Os {
    Windows,
    Linux,
    MacOs,
}

impl Os {
    /// The OS this binary was built for.
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        }
    }
}

/// One change to the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    WriteFile {
        path: PathBuf,
        contents: String,
    },
    RemoveFile {
        path: PathBuf,
    },
    /// Set the default value of a registry key (`HKCU\...`).
    RegSetDefault {
        key: String,
        value: String,
    },
    /// Delete a registry key (`HKCU\...`).
    RegDelete {
        key: String,
    },
}

/// The actions that register the host for the given extension ids.
pub fn plan_install(
    host_exe: &Path,
    os: Os,
    home: &Path,
    chrome_ids: &[String],
    firefox_ids: &[String],
) -> Vec<Action> {
    let chrome = chrome_manifest(host_exe, chrome_ids);
    let firefox = firefox_manifest(host_exe, firefox_ids);
    match os {
        Os::Windows => {
            let dir = exe_dir(host_exe);
            let chrome_path = dir.join(WINDOWS_CHROME_MANIFEST);
            let firefox_path = dir.join(WINDOWS_FIREFOX_MANIFEST);
            let mut actions = vec![
                Action::WriteFile { path: chrome_path.clone(), contents: chrome },
                Action::WriteFile { path: firefox_path.clone(), contents: firefox },
            ];
            for parent in WINDOWS_CHROME_KEYS {
                actions.push(Action::RegSetDefault {
                    key: registry_key(parent),
                    value: path_string(&chrome_path),
                });
            }
            actions.push(Action::RegSetDefault {
                key: registry_key(WINDOWS_FIREFOX_KEY),
                value: path_string(&firefox_path),
            });
            actions
        }
        Os::Linux | Os::MacOs => {
            let (chrome_dirs, firefox_dir) = unix_dirs(os, home);
            let mut actions: Vec<Action> = chrome_dirs
                .into_iter()
                .map(|dir| Action::WriteFile {
                    path: dir.join(MANIFEST_FILE),
                    contents: chrome.clone(),
                })
                .collect();
            actions.push(Action::WriteFile {
                path: firefox_dir.join(MANIFEST_FILE),
                contents: firefox,
            });
            actions
        }
    }
}

/// The actions that undo [`plan_install`].
pub fn plan_uninstall(host_exe: &Path, os: Os, home: &Path) -> Vec<Action> {
    match os {
        Os::Windows => {
            let dir = exe_dir(host_exe);
            let mut actions = vec![
                Action::RemoveFile { path: dir.join(WINDOWS_CHROME_MANIFEST) },
                Action::RemoveFile { path: dir.join(WINDOWS_FIREFOX_MANIFEST) },
            ];
            for parent in WINDOWS_CHROME_KEYS {
                actions.push(Action::RegDelete { key: registry_key(parent) });
            }
            actions.push(Action::RegDelete { key: registry_key(WINDOWS_FIREFOX_KEY) });
            actions
        }
        Os::Linux | Os::MacOs => {
            let (chrome_dirs, firefox_dir) = unix_dirs(os, home);
            chrome_dirs
                .into_iter()
                .chain(std::iter::once(firefox_dir))
                .map(|dir| Action::RemoveFile { path: dir.join(MANIFEST_FILE) })
                .collect()
        }
    }
}

/// Carry out `actions`, printing each one first. With `dry_run`, nothing
/// is changed.
pub fn apply(actions: &[Action], dry_run: bool) -> Result<()> {
    for action in actions {
        if dry_run {
            println!("[dry-run] {}", describe(action));
        } else {
            println!("{}", describe(action));
            run(action)?;
        }
    }
    Ok(())
}

fn describe(action: &Action) -> String {
    match action {
        Action::WriteFile { path, .. } => format!("write {}", path.display()),
        Action::RemoveFile { path } => format!("remove {}", path.display()),
        Action::RegSetDefault { key, value } => {
            format!("reg add \"{key}\" /ve /t REG_SZ /d \"{value}\" /f")
        }
        Action::RegDelete { key } => format!("reg delete \"{key}\" /f"),
    }
}

fn run(action: &Action) -> Result<()> {
    match action {
        Action::WriteFile { path, contents } => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))
        }
        Action::RemoveFile { path } => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("cannot remove {}", path.display())),
        },
        Action::RegSetDefault { key, value } => registry::set_default(key, value),
        Action::RegDelete { key } => registry::delete(key),
    }
}

/// The manifest for Chrome-family browsers.
fn chrome_manifest(host_exe: &Path, ids: &[String]) -> String {
    let origins: Vec<String> = ids.iter().map(|id| format!("chrome-extension://{id}/")).collect();
    manifest(host_exe, json!({ "allowed_origins": origins }))
}

/// The manifest for Firefox.
fn firefox_manifest(host_exe: &Path, ids: &[String]) -> String {
    manifest(host_exe, json!({ "allowed_extensions": ids }))
}

/// The fields shared by both manifests, plus `extra`, as pretty JSON.
fn manifest(host_exe: &Path, extra: serde_json::Value) -> String {
    let mut value = json!({
        "name": HOST_NAME,
        "description": DESCRIPTION,
        "path": path_string(host_exe),
        "type": "stdio",
    });
    if let (Some(base), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        for (key, field) in extra {
            base.insert(key.clone(), field.clone());
        }
    }
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    format!("{text}\n")
}

/// Chrome-family folders and the Firefox folder, for Linux or macOS.
fn unix_dirs(os: Os, home: &Path) -> (Vec<PathBuf>, PathBuf) {
    let (chrome, firefox) = match os {
        Os::MacOs => (&MACOS_CHROME_DIRS[..], MACOS_FIREFOX_DIR),
        Os::Linux | Os::Windows => (&LINUX_CHROME_DIRS[..], LINUX_FIREFOX_DIR),
    };
    (chrome.iter().map(|dir| home.join(dir)).collect(), home.join(firefox))
}

fn exe_dir(host_exe: &Path) -> &Path {
    host_exe.parent().unwrap_or(Path::new(""))
}

fn registry_key(parent: &str) -> String {
    format!("HKCU\\{parent}\\{HOST_NAME}")
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Registry access through reg.exe. Only Windows has a registry; elsewhere
/// these actions fail instead of doing nothing.
mod registry {
    use super::*;
    #[cfg(windows)]
    use std::process::{Command, Stdio};

    #[cfg(windows)]
    pub fn set_default(key: &str, value: &str) -> Result<()> {
        if reg(&["add", key, "/ve", "/t", "REG_SZ", "/d", value, "/f"])? {
            Ok(())
        } else {
            bail!("reg.exe could not set {key}")
        }
    }

    #[cfg(windows)]
    pub fn delete(key: &str) -> Result<()> {
        if reg(&["delete", key, "/f"])? {
            return Ok(());
        }
        // reg.exe also fails when the key is already gone, so only a key that
        // still exists is an error.
        if reg(&["query", key])? {
            bail!("reg.exe could not delete {key}");
        }
        Ok(())
    }

    /// Run reg.exe quietly; true when it exits successfully.
    #[cfg(windows)]
    fn reg(args: &[&str]) -> Result<bool> {
        let status = Command::new("reg.exe")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("cannot run reg.exe")?;
        Ok(status.success())
    }

    #[cfg(not(windows))]
    pub fn set_default(key: &str, _value: &str) -> Result<()> {
        bail!("cannot set {key}: the registry exists only on Windows")
    }

    #[cfg(not(windows))]
    pub fn delete(key: &str) -> Result<()> {
        bail!("cannot delete {key}: the registry exists only on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/fake/bin/vdm-nmhost.exe";
    const HOME: &str = "/fake/home";

    fn exe() -> PathBuf {
        PathBuf::from(EXE)
    }

    fn home() -> PathBuf {
        PathBuf::from(HOME)
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn written(actions: &[Action]) -> Vec<(PathBuf, String)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::WriteFile { path, contents } => Some((path.clone(), contents.clone())),
                _ => None,
            })
            .collect()
    }

    fn removed(actions: &[Action]) -> Vec<PathBuf> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::RemoveFile { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    fn reg_sets(actions: &[Action]) -> Vec<(String, String)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::RegSetDefault { key, value } => Some((key.clone(), value.clone())),
                _ => None,
            })
            .collect()
    }

    fn reg_deletes(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::RegDelete { key } => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn windows_install_writes_manifests_next_to_exe() {
        let actions = plan_install(
            &exe(),
            Os::Windows,
            &home(),
            &ids(&["abc", "def"]),
            &ids(&["vdm@vdm.local"]),
        );
        let files = written(&actions);
        let dir = Path::new("/fake/bin");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, dir.join("com.vdm.native.chrome.json"));
        assert_eq!(files[1].0, dir.join("com.vdm.native.firefox.json"));

        let chrome: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(chrome["name"], HOST_NAME);
        assert_eq!(chrome["description"], DESCRIPTION);
        assert_eq!(chrome["path"], EXE);
        assert_eq!(chrome["type"], "stdio");
        assert_eq!(
            chrome["allowed_origins"],
            json!(["chrome-extension://abc/", "chrome-extension://def/"])
        );
        assert!(chrome.get("allowed_extensions").is_none());

        let firefox: serde_json::Value = serde_json::from_str(&files[1].1).unwrap();
        assert_eq!(firefox["allowed_extensions"], json!(["vdm@vdm.local"]));
        assert!(firefox.get("allowed_origins").is_none());
        assert_eq!(firefox["path"], EXE);
    }

    #[test]
    fn windows_install_registers_every_browser() {
        let actions =
            plan_install(&exe(), Os::Windows, &home(), &ids(&["abc"]), &ids(&["vdm@vdm.local"]));
        let chrome_path = Path::new("/fake/bin").join("com.vdm.native.chrome.json");
        let firefox_path = Path::new("/fake/bin").join("com.vdm.native.firefox.json");
        let chrome_value = chrome_path.to_string_lossy().into_owned();
        let firefox_value = firefox_path.to_string_lossy().into_owned();

        let sets = reg_sets(&actions);
        assert_eq!(
            sets,
            vec![
                (
                    r"HKCU\Software\Google\Chrome\NativeMessagingHosts\com.vdm.native".into(),
                    chrome_value.clone()
                ),
                (
                    r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\com.vdm.native".into(),
                    chrome_value.clone()
                ),
                (
                    r"HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.vdm.native"
                        .into(),
                    chrome_value.clone()
                ),
                (
                    r"HKCU\Software\Chromium\NativeMessagingHosts\com.vdm.native".into(),
                    chrome_value
                ),
                (
                    r"HKCU\Software\Mozilla\NativeMessagingHosts\com.vdm.native".into(),
                    firefox_value
                ),
            ]
        );
        // Files are written before the registry points at them.
        assert!(matches!(actions[0], Action::WriteFile { .. }));
        assert!(matches!(actions[1], Action::WriteFile { .. }));
        assert!(matches!(actions[2], Action::RegSetDefault { .. }));
    }

    #[test]
    fn linux_install_writes_each_browser_folder() {
        let actions = plan_install(&exe(), Os::Linux, &home(), &ids(&["abc"]), &ids(&["ff"]));
        assert!(reg_sets(&actions).is_empty());
        let home = home();
        let files = written(&actions);
        let paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            paths,
            vec![
                home.join(".config/google-chrome/NativeMessagingHosts/com.vdm.native.json"),
                home.join(".config/chromium/NativeMessagingHosts/com.vdm.native.json"),
                home.join(".config/microsoft-edge/NativeMessagingHosts/com.vdm.native.json"),
                home.join(
                    ".config/BraveSoftware/Brave-Browser/NativeMessagingHosts/com.vdm.native.json"
                ),
                home.join(".mozilla/native-messaging-hosts/com.vdm.native.json"),
            ]
        );
        let chrome: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(chrome["allowed_origins"], json!(["chrome-extension://abc/"]));
        let firefox: serde_json::Value = serde_json::from_str(&files[4].1).unwrap();
        assert_eq!(firefox["allowed_extensions"], json!(["ff"]));
    }

    #[test]
    fn macos_install_writes_each_browser_folder() {
        let actions = plan_install(&exe(), Os::MacOs, &home(), &ids(&["abc"]), &ids(&["ff"]));
        let home = home();
        let paths: Vec<PathBuf> = written(&actions).into_iter().map(|(p, _)| p).collect();
        assert_eq!(
            paths,
            vec![
                home.join("Library/Application Support/Google/Chrome/NativeMessagingHosts/com.vdm.native.json"),
                home.join("Library/Application Support/Chromium/NativeMessagingHosts/com.vdm.native.json"),
                home.join("Library/Application Support/Microsoft Edge/NativeMessagingHosts/com.vdm.native.json"),
                home.join("Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts/com.vdm.native.json"),
                home.join("Library/Application Support/Mozilla/NativeMessagingHosts/com.vdm.native.json"),
            ]
        );
    }

    #[test]
    fn every_chrome_id_is_allowed() {
        let actions = plan_install(
            &exe(),
            Os::Linux,
            &home(),
            &ids(&["first", "second", DEFAULT_CHROME_ID]),
            &ids(&[DEFAULT_FIREFOX_ID]),
        );
        let (_, contents) = &written(&actions)[0];
        let manifest: serde_json::Value = serde_json::from_str(contents).unwrap();
        assert_eq!(
            manifest["allowed_origins"],
            json!([
                "chrome-extension://first/",
                "chrome-extension://second/",
                format!("chrome-extension://{DEFAULT_CHROME_ID}/"),
            ])
        );
    }

    #[test]
    fn uninstall_removes_exactly_what_install_wrote() {
        for os in [Os::Windows, Os::Linux, Os::MacOs] {
            let install = plan_install(&exe(), os, &home(), &ids(&["abc"]), &ids(&["ff"]));
            let uninstall = plan_uninstall(&exe(), os, &home());

            let mut written_paths: Vec<PathBuf> =
                written(&install).into_iter().map(|(p, _)| p).collect();
            let mut removed_paths = removed(&uninstall);
            written_paths.sort();
            removed_paths.sort();
            assert_eq!(removed_paths, written_paths, "{os:?}");

            let mut set_keys: Vec<String> =
                reg_sets(&install).into_iter().map(|(k, _)| k).collect();
            let mut deleted_keys = reg_deletes(&uninstall);
            set_keys.sort();
            deleted_keys.sort();
            assert_eq!(deleted_keys, set_keys, "{os:?}");
        }
    }

    #[test]
    fn dry_run_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let new_file = dir.path().join("sub/new.json");
        let existing = dir.path().join("old.json");
        fs::write(&existing, "keep").unwrap();
        let actions = vec![
            Action::WriteFile { path: new_file.clone(), contents: "x".into() },
            Action::RemoveFile { path: existing.clone() },
            Action::RegSetDefault { key: "HKCU\\Software\\x".into(), value: "v".into() },
            Action::RegDelete { key: "HKCU\\Software\\x".into() },
        ];
        apply(&actions, true).unwrap();
        assert!(!new_file.exists());
        assert!(!new_file.parent().unwrap().exists());
        assert_eq!(fs::read_to_string(&existing).unwrap(), "keep");
    }

    #[test]
    fn apply_writes_and_removes_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/com.vdm.native.json");
        let install = vec![Action::WriteFile { path: path.clone(), contents: "{}\n".into() }];
        apply(&install, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}\n");

        let uninstall = vec![Action::RemoveFile { path: path.clone() }];
        apply(&uninstall, false).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn removing_a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-installed.json");
        apply(&[Action::RemoveFile { path: missing }], false).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn registry_actions_fail_outside_windows() {
        let actions = [Action::RegDelete { key: "HKCU\\Software\\x".into() }];
        assert!(apply(&actions, false).is_err());
    }
}
