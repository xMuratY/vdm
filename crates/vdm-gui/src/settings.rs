//! User settings: the TOML settings file, the Settings form's text fields,
//! the speed parser, and the app's file locations. No iced here, so it is
//! unit-testable as plain Rust.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::Context;
use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};
use vdm_core::MAX_CONNECTIONS;

/// Highest allowed number of downloads running at the same time.
const MAX_ACTIVE: usize = 16;

/// User settings, stored as TOML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)] // missing keys take their defaults, unknown keys are ignored
pub struct Settings {
    /// Where new downloads are saved.
    pub download_dir: PathBuf,
    /// Downloads running at the same time, 1..=16.
    pub max_active: usize,
    /// Connections per new download, 1..=vdm_core::MAX_CONNECTIONS.
    pub connections: u8,
    /// Global speed limit in bytes per second; None = unlimited.
    pub speed_limit: Option<u64>,
    /// Offer to download links to files copied to the clipboard.
    pub watch_clipboard: bool,
    /// Offer copied links even while the window is minimized or in the tray
    /// (it comes forward with the Add dialog).
    pub clipboard_always: bool,
    /// Downloads the browser extension hands over open in the Add dialog first.
    pub ask_browser_downloads: bool,
    /// Torrents never upload to other peers, and don't seed when done (the default).
    pub torrent_download_only: bool,
    /// Show a desktop notification when a download finishes or fails.
    pub notify: bool,
    /// Closing the window hides it to the tray instead of quitting (where a tray exists).
    pub close_to_tray: bool,
    /// Save into a subfolder per file type (Video, Music, ...) when using the default folder.
    pub use_categories: bool,
    /// Proxy: "" or "system" (the system's), "none", or a URL like socks5://host:1080.
    pub proxy: String,
    /// Command run after each download, e.g. a virus scan; `{path}` is the file.
    pub after_download: String,
    /// Check for new versions at startup and once a day.
    pub check_updates: bool,
    /// Versions the user chose not to install.
    pub skipped_versions: Vec<String>,
    /// Saved site logins; their passwords are in the OS credential store.
    pub logins: Vec<crate::logins::LoginEntry>,
    /// Light, dark, or follow the system.
    pub theme: crate::theme::ThemeChoice,
    /// The interface language; None follows the system.
    pub language: Option<crate::i18n::Lang>,
    /// The download list's columns.
    pub columns: crate::columns::Columns,
    /// The details panel is open beside the list.
    pub details_panel: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            max_active: 3,
            connections: 8,
            speed_limit: None,
            watch_clipboard: true,
            clipboard_always: false,
            ask_browser_downloads: true,
            torrent_download_only: true,
            notify: true,
            close_to_tray: true,
            use_categories: true,
            proxy: String::new(),
            after_download: String::new(),
            check_updates: true,
            skipped_versions: Vec::new(),
            logins: Vec::new(),
            theme: crate::theme::ThemeChoice::System,
            language: None,
            columns: crate::columns::Columns::default(),
            details_panel: false,
        }
    }
}

/// The user's Downloads folder, falling back to `~/Downloads`, then `.`.
fn default_download_dir() -> PathBuf {
    match UserDirs::new() {
        Some(dirs) => dirs
            .download_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dirs.home_dir().join("Downloads")),
        None => PathBuf::from("."),
    }
}

impl Settings {
    /// Read `path`. A missing file gives the defaults. Invalid TOML is an error
    /// that names the path. The result is passed through `validated()`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("cannot read settings file {}", path.display()));
            }
        };
        let settings: Self = toml::from_str(&text)
            .with_context(|| format!("invalid settings file {}", path.display()))?;
        Ok(settings.validated())
    }

    /// Write `path` atomically (temp file in the same folder + rename),
    /// creating the parent folder if needed.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self).context("cannot encode settings")?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create folder {}", parent.display()))?;
        }
        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp = PathBuf::from(tmp_name);
        fs::write(&tmp, text)
            .with_context(|| format!("cannot write settings file {}", tmp.display()))?;
        if let Err(err) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(err)
                .with_context(|| format!("cannot replace settings file {}", path.display()));
        }
        Ok(())
    }

    /// Clamp max_active to 1..=16 and connections to 1..=MAX_CONNECTIONS;
    /// Some(0) speed limit becomes None; an empty download_dir becomes the default one.
    pub fn validated(mut self) -> Self {
        self.max_active = self.max_active.clamp(1, MAX_ACTIVE);
        self.connections = self.connections.clamp(1, MAX_CONNECTIONS);
        if self.speed_limit == Some(0) {
            self.speed_limit = None;
        }
        if self.download_dir.as_os_str().is_empty() {
            self.download_dir = default_download_dir();
        }
        self.columns = std::mem::take(&mut self.columns).validated();
        self
    }
}

/// Where the app keeps its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub settings: PathBuf, // <config dir>/settings.toml
    pub database: PathBuf, // <data dir>/vdm.db
}

impl Paths {
    /// Standard per-user locations: `directories::ProjectDirs::from("", "", "VDM")`
    /// config_dir() and data_dir(). Creates both folders. Errors if no home folder.
    pub fn discover() -> anyhow::Result<Self> {
        let dirs = ProjectDirs::from("", "", "VDM")
            .context("no home folder found, cannot locate the app's files")?;
        let config = dirs.config_dir();
        let data = dirs.data_dir();
        fs::create_dir_all(config)
            .with_context(|| format!("cannot create folder {}", config.display()))?;
        fs::create_dir_all(data)
            .with_context(|| format!("cannot create folder {}", data.display()))?;
        Ok(Self { settings: config.join("settings.toml"), database: data.join("vdm.db") })
    }

    /// Everything inside one folder (for `--data-dir` and tests): dir/settings.toml, dir/vdm.db.
    /// Creates the folder.
    pub fn in_dir(dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("cannot create folder {}", dir.display()))?;
        Ok(Self { settings: dir.join("settings.toml"), database: dir.join("vdm.db") })
    }
}

/// The Settings form's text fields, as the user types them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SettingsForm {
    pub download_dir: String,
    pub max_active: String,
    pub connections: String,
    /// Empty = unlimited; otherwise a number with an optional K/M/G suffix (binary multiples), e.g. "500K", "1.5M".
    pub speed_limit: String,
    pub watch_clipboard: bool,
    pub clipboard_always: bool,
    pub ask_browser_downloads: bool,
    pub torrent_download_only: bool,
    pub notify: bool,
    pub close_to_tray: bool,
    pub use_categories: bool,
    /// "", "system", "none", or a proxy URL.
    pub proxy: String,
    pub after_download: String,
    pub check_updates: bool,
    pub theme: crate::theme::ThemeChoice,
    pub language: Option<crate::i18n::Lang>,
    /// Start at sign-in. Not part of `Settings`: the system holds it.
    pub launch_at_startup: bool,
}

impl SettingsForm {
    /// Fill the fields from saved settings. The speed uses the largest suffix
    /// that keeps it a whole number.
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            download_dir: s.download_dir.to_string_lossy().into_owned(),
            max_active: s.max_active.to_string(),
            connections: s.connections.to_string(),
            speed_limit: s.speed_limit.map(format_speed).unwrap_or_default(),
            watch_clipboard: s.watch_clipboard,
            clipboard_always: s.clipboard_always,
            ask_browser_downloads: s.ask_browser_downloads,
            torrent_download_only: s.torrent_download_only,
            notify: s.notify,
            close_to_tray: s.close_to_tray,
            use_categories: s.use_categories,
            proxy: s.proxy.clone(),
            after_download: s.after_download.clone(),
            check_updates: s.check_updates,
            theme: s.theme,
            language: s.language,
            launch_at_startup: false,
        }
    }

    /// Parse every field. On the first invalid field return Err with a short
    /// message naming it, e.g. "Max downloads must be a whole number from 1 to 16".
    /// Out-of-range numbers are errors here (not clamped). Leading/trailing spaces are ignored.
    /// download_dir must be non-empty.
    pub fn parse(&self) -> Result<Settings, String> {
        let download_dir = self.download_dir.trim();
        if download_dir.is_empty() {
            return Err(crate::i18n::t!("settings.err_folder_empty"));
        }

        let max_active = self
            .max_active
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_ACTIVE).contains(n))
            .ok_or_else(|| crate::i18n::t!("settings.err_max_active", max = MAX_ACTIVE))?;

        let connections = self
            .connections
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=MAX_CONNECTIONS).contains(n))
            .ok_or_else(|| crate::i18n::t!("settings.err_connections", max = MAX_CONNECTIONS))?;

        let proxy = self.proxy.trim().to_owned();
        proxy
            .parse::<vdm_core::ProxySetting>()
            .map_err(|err| crate::i18n::t!("settings.err_proxy", error = err))?;
        let after_download = self.after_download.trim().to_owned();
        let speed = self.speed_limit.trim();
        let speed_limit = if speed.is_empty() {
            None
        } else {
            Some(
                parse_speed(speed)
                    .map_err(|err| crate::i18n::t!("settings.err_speed", error = err))?,
            )
        };

        Ok(Settings {
            download_dir: PathBuf::from(download_dir),
            max_active,
            connections,
            speed_limit,
            watch_clipboard: self.watch_clipboard,
            clipboard_always: self.clipboard_always,
            ask_browser_downloads: self.ask_browser_downloads,
            torrent_download_only: self.torrent_download_only,
            notify: self.notify,
            close_to_tray: self.close_to_tray,
            use_categories: self.use_categories,
            proxy,
            after_download,
            check_updates: self.check_updates,
            // Logins and skipped versions are kept by the caller.
            skipped_versions: Vec::new(),
            logins: Vec::new(),
            theme: self.theme,
            language: self.language,
            // Kept by the caller, like the logins.
            columns: crate::columns::Columns::default(),
            details_panel: false,
        })
    }
}

/// Bytes per second as the form shows it: the largest K/M/G suffix that
/// divides it evenly, or plain bytes.
pub fn format_speed(bytes: u64) -> String {
    for (unit, suffix) in [(1u64 << 30, "G"), (1 << 20, "M"), (1 << 10, "K")] {
        if bytes >= unit && bytes.is_multiple_of(unit) {
            return format!("{}{suffix}", bytes / unit);
        }
    }
    bytes.to_string()
}

/// "123", "500K", "1.5m", "2G" → bytes/s; must be > 0. Same rules as the CLI's parse_rate.
pub fn parse_speed(s: &str) -> Result<u64, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn good_form() -> SettingsForm {
        SettingsForm {
            download_dir: "D:/Downloads".into(),
            max_active: "3".into(),
            connections: "8".into(),
            speed_limit: String::new(),
            ..SettingsForm::default()
        }
    }

    #[test]
    fn defaults() {
        let s = Settings::default();
        assert_eq!(s.max_active, 3);
        assert_eq!(s.connections, 8);
        assert_eq!(s.speed_limit, None);
        assert!(!s.download_dir.as_os_str().is_empty());
    }

    #[test]
    fn load_missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let s = Settings {
            download_dir: PathBuf::from("D:/Games/Downloads"),
            max_active: 7,
            connections: 16,
            speed_limit: Some(512 * 1024),
            ..Settings::default()
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);
    }

    #[test]
    fn save_creates_parent_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("settings.toml");
        Settings::default().save(&path).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn saved_file_is_toml_with_expected_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let s = Settings { speed_limit: Some(1024), ..Settings::default() };
        s.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let table = toml::from_str::<toml::Table>(&text).unwrap();
        for key in ["download_dir", "max_active", "connections", "speed_limit"] {
            assert!(table.contains_key(key), "missing key {key}");
        }
        assert_eq!(table.get("max_active").and_then(|v| v.as_integer()), Some(3));
        assert_eq!(table.get("connections").and_then(|v| v.as_integer()), Some(8));
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        Settings::default().save(&path).unwrap();
        Settings::default().save(&path).unwrap();
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["settings.toml"]);
    }

    #[test]
    fn load_ignores_unknown_keys_and_fills_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        fs::write(&path, "max_active = 5\nmystery = true\n").unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.max_active, 5);
        assert_eq!(s.connections, 8);
        assert_eq!(s.speed_limit, None);
    }

    #[test]
    fn load_invalid_toml_is_error_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        fs::write(&path, "max_active = [").unwrap();
        let err = Settings::load(&path).unwrap_err();
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "error does not name the path: {err}"
        );
    }

    #[test]
    fn load_clamps_out_of_range_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        fs::write(&path, "max_active = 99\nconnections = 0\nspeed_limit = 0\n").unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.max_active, 16);
        assert_eq!(s.connections, 1);
        assert_eq!(s.speed_limit, None);
    }

    #[test]
    fn validated_clamps_max_active() {
        let low = Settings { max_active: 0, ..Settings::default() }.validated();
        let high = Settings { max_active: 17, ..Settings::default() }.validated();
        let ok = Settings { max_active: 16, ..Settings::default() }.validated();
        assert_eq!(low.max_active, 1);
        assert_eq!(high.max_active, 16);
        assert_eq!(ok.max_active, 16);
    }

    #[test]
    fn validated_clamps_connections() {
        let low = Settings { connections: 0, ..Settings::default() }.validated();
        let high = Settings { connections: 200, ..Settings::default() }.validated();
        assert_eq!(low.connections, 1);
        assert_eq!(high.connections, MAX_CONNECTIONS);
    }

    #[test]
    fn validated_zero_speed_is_unlimited() {
        let zero = Settings { speed_limit: Some(0), ..Settings::default() }.validated();
        let some = Settings { speed_limit: Some(5), ..Settings::default() }.validated();
        assert_eq!(zero.speed_limit, None);
        assert_eq!(some.speed_limit, Some(5));
    }

    #[test]
    fn validated_empty_download_dir_becomes_default() {
        let s = Settings { download_dir: PathBuf::new(), ..Settings::default() }.validated();
        assert_eq!(s.download_dir, Settings::default().download_dir);
        assert!(!s.download_dir.as_os_str().is_empty());
    }

    #[test]
    fn validated_keeps_valid_settings_unchanged() {
        let s = Settings {
            download_dir: PathBuf::from("E:/dl"),
            max_active: 4,
            connections: 12,
            speed_limit: Some(1000),
            ..Settings::default()
        };
        assert_eq!(s.clone().validated(), s);
    }

    #[test]
    fn paths_in_dir_creates_folder_and_returns_paths() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("data");
        let paths = Paths::in_dir(&inner).unwrap();
        assert!(inner.is_dir());
        assert_eq!(paths.settings, inner.join("settings.toml"));
        assert_eq!(paths.database, inner.join("vdm.db"));
    }

    #[test]
    fn paths_discover_names_the_files() {
        let paths = Paths::discover().unwrap();
        assert_eq!(paths.settings.file_name().and_then(|n| n.to_str()), Some("settings.toml"));
        assert_eq!(paths.database.file_name().and_then(|n| n.to_str()), Some("vdm.db"));
    }

    #[test]
    fn form_from_settings_formats_speed() {
        let speed = |bytes: Option<u64>| {
            SettingsForm::from_settings(&Settings { speed_limit: bytes, ..Settings::default() })
                .speed_limit
        };
        assert_eq!(speed(None), "");
        assert_eq!(speed(Some(1500)), "1500");
        assert_eq!(speed(Some(524_288)), "512K");
        assert_eq!(speed(Some(1_048_576)), "1M");
        assert_eq!(speed(Some(3 * 1024 * 1024 * 1024)), "3G");
    }

    #[test]
    fn form_parse_happy_path() {
        let form = SettingsForm {
            download_dir: "D:/Games".into(),
            max_active: "4".into(),
            connections: "16".into(),
            speed_limit: "1.5M".into(),
            ..SettingsForm::default()
        };
        let s = form.parse().unwrap();
        assert_eq!(s.download_dir, PathBuf::from("D:/Games"));
        assert_eq!(s.max_active, 4);
        assert_eq!(s.connections, 16);
        assert_eq!(s.speed_limit, Some(1_572_864));
    }

    #[test]
    fn form_empty_speed_is_unlimited() {
        assert_eq!(good_form().parse().unwrap().speed_limit, None);
    }

    #[test]
    fn form_parse_errors_name_the_field() {
        let cases = [
            (SettingsForm { download_dir: "   ".into(), ..good_form() }, "Download folder"),
            (SettingsForm { max_active: "abc".into(), ..good_form() }, "Max downloads"),
            (SettingsForm { max_active: "0".into(), ..good_form() }, "Max downloads"),
            (SettingsForm { max_active: "17".into(), ..good_form() }, "Max downloads"),
            (SettingsForm { connections: "33".into(), ..good_form() }, "Connections"),
            (SettingsForm { connections: "0".into(), ..good_form() }, "Connections"),
            (SettingsForm { connections: "many".into(), ..good_form() }, "Connections"),
            (SettingsForm { speed_limit: "fast".into(), ..good_form() }, "Speed limit"),
            (SettingsForm { speed_limit: "0".into(), ..good_form() }, "Speed limit"),
            (SettingsForm { speed_limit: "0K".into(), ..good_form() }, "Speed limit"),
        ];
        for (form, needle) in cases {
            let err = form.parse().unwrap_err();
            assert!(err.contains(needle), "{err:?} does not mention {needle:?}");
        }
    }

    #[test]
    fn form_ignores_surrounding_spaces() {
        let form = SettingsForm {
            download_dir: "  D:/dl  ".into(),
            max_active: " 2 ".into(),
            connections: "\t5 ".into(),
            speed_limit: "  500K  ".into(),
            ..SettingsForm::default()
        };
        let s = form.parse().unwrap();
        assert_eq!(s.download_dir, PathBuf::from("D:/dl"));
        assert_eq!(s.max_active, 2);
        assert_eq!(s.connections, 5);
        assert_eq!(s.speed_limit, Some(512_000));
    }

    #[test]
    fn form_round_trip() {
        let s = Settings {
            download_dir: PathBuf::from("D:/dl"),
            max_active: 9,
            connections: 32,
            speed_limit: Some(3 * 1024 * 1024),
            watch_clipboard: false,
            clipboard_always: true,
            ask_browser_downloads: false,
            torrent_download_only: false,
            notify: false,
            close_to_tray: false,
            use_categories: false,
            proxy: "socks5://127.0.0.1:1080".into(),
            after_download: r#""C:\AV\scan.exe" "{path}""#.into(),
            check_updates: false,
            skipped_versions: Vec::new(),
            logins: Vec::new(),
            theme: crate::theme::ThemeChoice::Dark,
            language: Some(crate::i18n::Lang::Ja),
            columns: crate::columns::Columns::default(),
            details_panel: false,
        };
        assert_eq!(SettingsForm::from_settings(&s).parse().unwrap(), s);
        let bad_proxy = SettingsForm { proxy: "ftp://x".into(), ..SettingsForm::from_settings(&s) };
        assert!(bad_proxy.parse().unwrap_err().starts_with("Proxy:"));

        let unlimited = Settings { speed_limit: None, ..s };
        assert_eq!(SettingsForm::from_settings(&unlimited).parse().unwrap(), unlimited);
    }

    #[test]
    fn parse_speed_like_the_cli() {
        assert_eq!(parse_speed("1000").unwrap(), 1000);
        assert_eq!(parse_speed("500K").unwrap(), 500 * 1024);
        assert_eq!(parse_speed("1.5m").unwrap(), 1_572_864);
        assert_eq!(parse_speed("2G").unwrap(), 2 << 30);
        assert_eq!(parse_speed(" 2k ").unwrap(), 2048);
        assert!(parse_speed("0").is_err());
        assert!(parse_speed("-5").is_err());
        assert!(parse_speed("5X").is_err());
        assert!(parse_speed("fast").is_err());
        assert!(parse_speed("").is_err());
        assert!(parse_speed("K").is_err());
    }
}
