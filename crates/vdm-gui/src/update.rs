//! Auto-update logic: the release manifest (`latest.json`), choosing the
//! update to offer, and the command that installs it. No networking and no
//! iced here, so it is unit-testable as plain Rust.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use semver::Version;

/// How often to check for updates: once a day.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long to wait after startup before the first check.
pub const STARTUP_DELAY: Duration = Duration::from_secs(30);

/// One installer in the manifest, for one platform.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
}

/// The release manifest published next to the installers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub assets: BTreeMap<String, Asset>,
}

/// An update this build can install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub version: Version,
    pub notes: Option<String>,
    pub asset: Asset,
}

/// The asset key for this build: "<os>-<arch>", e.g. "windows-x86_64".
pub fn platform_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Parse a version, accepting a leading "v" ("v0.2.0").
fn parse_version(text: &str) -> Result<Version, semver::Error> {
    Version::parse(text.strip_prefix('v').unwrap_or(text))
}

/// True for `https://...`, and for `http://` only on 127.0.0.1 or localhost (tests).
fn url_allowed(url: &str) -> bool {
    if url.strip_prefix("https://").is_some_and(|rest| !rest.is_empty()) {
        return true;
    }
    ["http://127.0.0.1", "http://localhost"].iter().any(|host| {
        url.strip_prefix(host)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', ':', '?', '#']))
    })
}

/// Check one asset's URL and sha256 (64 hex digits).
fn validate_asset(key: &str, asset: &Asset) -> Result<(), String> {
    if !url_allowed(&asset.url) {
        return Err(format!("asset {key}: URL must start with https://"));
    }
    if asset.sha256.len() != 64 || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("asset {key}: sha256 must be 64 hex digits"));
    }
    Ok(())
}

/// Parse a manifest. Errors on invalid JSON, a version that is not semver,
/// an asset URL that is not https (http only for 127.0.0.1/localhost), or a
/// sha256 that is not 64 hex digits. The sha256 is stored in lowercase.
pub fn parse_manifest(json: &str) -> Result<Manifest, String> {
    let mut manifest: Manifest =
        serde_json::from_str(json).map_err(|err| format!("invalid manifest JSON: {err}"))?;
    parse_version(&manifest.version)
        .map_err(|err| format!("version {:?} is not semver: {err}", manifest.version))?;
    for (key, asset) in &mut manifest.assets {
        validate_asset(key, asset)?;
        asset.sha256.make_ascii_lowercase();
    }
    Ok(manifest)
}

/// The update to offer, if any. The manifest's version must be strictly newer
/// than `current` (semver precedence). Pre-releases are offered only when
/// `current` is a pre-release too or `allow_prerelease` is true. The asset for
/// `platform` must exist. Versions in `skipped` are not offered.
pub fn check(
    manifest: &Manifest,
    current: &Version,
    platform: &str,
    allow_prerelease: bool,
    skipped: &[String],
) -> Result<Option<Available>, String> {
    let version = parse_version(&manifest.version)
        .map_err(|err| format!("version {:?} is not semver: {err}", manifest.version))?;
    if !version.pre.is_empty() && !allow_prerelease && current.pre.is_empty() {
        return Ok(None);
    }
    if version <= *current {
        return Ok(None);
    }
    if skipped.iter().filter_map(|text| parse_version(text).ok()).any(|skip| skip == version) {
        return Ok(None);
    }
    let Some(asset) = manifest.assets.get(platform) else {
        return Ok(None);
    };
    validate_asset(platform, asset)?;
    Ok(Some(Available { version, notes: manifest.notes.clone(), asset: asset.clone() }))
}

/// The current build's version (CARGO_PKG_VERSION).
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is valid semver")
}

/// How to install `msi` silently and start VDM again afterwards, as (program, args).
/// Windows only; None elsewhere for now. The relaunch waits for msiexec because
/// it runs after `&&`.
///
/// It first waits about two seconds so VDM can exit and release its files.
/// The single argument after `/c` is
/// `"ping -n 3 127.0.0.1 >nul & msiexec /i "<msi>" /qn /norestart /l*v "<log>" && start "" "<exe>""`,
/// with the outer quotes that `cmd /c` expects. Pass it verbatim with
/// `CommandExt::raw_arg` (not `Command::args`): std escapes the inner quotes,
/// and cmd does not understand that escaping.
pub fn install_command(msi: &Path, log: &Path, relaunch: &Path) -> Option<(String, Vec<String>)> {
    if !cfg!(windows) {
        return None;
    }
    let line = format!(
        "\"ping -n 3 127.0.0.1 >nul & msiexec /i \"{}\" /qn /norestart /l*v \"{}\" && start \"\" \"{}\"\"",
        msi.display(),
        log.display(),
        relaunch.display(),
    );
    Some(("cmd".to_string(), vec!["/d".to_string(), "/c".to_string(), line]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn manifest_json(version: &str) -> String {
        format!(
            r#"{{"version": "{version}", "assets": {{
                "windows-x86_64": {{ "url": "https://example.com/vdm.msi", "sha256": "{SHA}" }}
            }}}}"#
        )
    }

    fn manifest(version: &str) -> Manifest {
        parse_manifest(&manifest_json(version)).unwrap()
    }

    fn asset_json(url: &str, sha: &str) -> String {
        format!(
            r#"{{"version": "0.2.0", "assets": {{"windows-x86_64": {{"url": "{url}", "sha256": "{sha}"}}}}}}"#
        )
    }

    #[test]
    fn parse_full_example() {
        let json = r#"{
            "version": "0.2.0",
            "published": "2026-11-01T12:00:00Z",
            "notes": "Fixes.\nMore fixes.",
            "assets": {
                "windows-x86_64": { "url": "https://example.com/vdm-0.2.0-x86_64.msi", "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", "size": 12345678 },
                "windows-aarch64": { "url": "https://example.com/vdm-0.2.0-aarch64.msi", "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", "size": 1 }
            }
        }"#;
        let m = parse_manifest(json).unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.published.as_deref(), Some("2026-11-01T12:00:00Z"));
        assert_eq!(m.notes.as_deref(), Some("Fixes.\nMore fixes."));
        assert_eq!(m.assets.len(), 2);
        let x64 = &m.assets["windows-x86_64"];
        assert_eq!(x64.url, "https://example.com/vdm-0.2.0-x86_64.msi");
        assert_eq!(x64.size, Some(12345678));
    }

    #[test]
    fn parse_minimal() {
        let m = parse_manifest(r#"{"version": "1.0.0"}"#).unwrap();
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.published, None);
        assert_eq!(m.notes, None);
        assert!(m.assets.is_empty());
    }

    #[test]
    fn parse_ignores_unknown_fields() {
        let json = r#"{"version": "1.0.0", "channel": "stable", "extra": {"a": 1},
            "assets": {"windows-x86_64": {"url": "https://e.com/a.msi", "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", "signature": "x"}}}"#;
        let m = parse_manifest(json).unwrap();
        assert_eq!(m.assets["windows-x86_64"].size, None);
    }

    #[test]
    fn parse_accepts_v_prefix() {
        let m = parse_manifest(&manifest_json("v0.2.0")).unwrap();
        assert_eq!(m.version, "v0.2.0");
        let current = Version::parse("0.1.0").unwrap();
        let offered = check(&m, &current, "windows-x86_64", false, &[]).unwrap();
        assert_eq!(offered.map(|a| a.version), Some(Version::parse("0.2.0").unwrap()));
    }

    #[test]
    fn parse_rejects_invalid_semver() {
        assert!(parse_manifest(r#"{"version": "two"}"#).is_err());
        assert!(parse_manifest(r#"{"version": "1.0"}"#).is_err());
        assert!(parse_manifest(r#"{"version": ""}"#).is_err());
    }

    #[test]
    fn parse_rejects_invalid_json() {
        assert!(parse_manifest("{not json").is_err());
        assert!(parse_manifest(r#"{"notes": "no version"}"#).is_err());
    }

    #[test]
    fn parse_rejects_bad_url_schemes() {
        assert!(parse_manifest(&asset_json("ftp://example.com/x.msi", SHA)).is_err());
        assert!(parse_manifest(&asset_json("http://example.com/x.msi", SHA)).is_err());
        assert!(parse_manifest(&asset_json("http://localhost.example.com/x.msi", SHA)).is_err());
        assert!(parse_manifest(&asset_json("https://", SHA)).is_err());
        assert!(parse_manifest(&asset_json("example.com/x.msi", SHA)).is_err());
    }

    #[test]
    fn parse_accepts_local_http() {
        assert!(parse_manifest(&asset_json("http://127.0.0.1:8080/x.msi", SHA)).is_ok());
        assert!(parse_manifest(&asset_json("http://localhost/x.msi", SHA)).is_ok());
    }

    #[test]
    fn parse_rejects_bad_sha256() {
        let short = "0".repeat(63);
        let not_hex = "g".repeat(64);
        assert!(parse_manifest(&asset_json("https://e.com/x.msi", &short)).is_err());
        assert!(parse_manifest(&asset_json("https://e.com/x.msi", &not_hex)).is_err());
        assert!(parse_manifest(&asset_json("https://e.com/x.msi", "")).is_err());
    }

    #[test]
    fn parse_accepts_uppercase_hex() {
        let upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789";
        let m = parse_manifest(&asset_json("https://e.com/x.msi", upper)).unwrap();
        assert_eq!(m.assets["windows-x86_64"].sha256, upper.to_ascii_lowercase());
    }

    #[test]
    fn check_newer_equal_older() {
        let current = Version::parse("0.2.0").unwrap();
        let newer = check(&manifest("0.3.0"), &current, "windows-x86_64", false, &[]).unwrap();
        let available = newer.expect("newer version is offered");
        assert_eq!(available.version, Version::parse("0.3.0").unwrap());
        assert_eq!(available.asset.url, "https://example.com/vdm.msi");

        assert_eq!(
            check(&manifest("0.2.0"), &current, "windows-x86_64", false, &[]).unwrap(),
            None
        );
        assert_eq!(
            check(&manifest("0.1.9"), &current, "windows-x86_64", false, &[]).unwrap(),
            None
        );
    }

    #[test]
    fn check_prerelease_rules() {
        let stable = Version::parse("0.2.0").unwrap();
        let beta = Version::parse("0.3.0-beta.1").unwrap();

        // A stable build is not offered a pre-release unless allowed.
        assert_eq!(
            check(&manifest("0.3.0-beta.1"), &stable, "windows-x86_64", false, &[]).unwrap(),
            None
        );
        assert!(
            check(&manifest("0.3.0-beta.1"), &stable, "windows-x86_64", true, &[])
                .unwrap()
                .is_some()
        );

        // A pre-release build is offered pre-releases, and the stable release of the same version.
        assert!(
            check(&manifest("0.3.0-beta.2"), &beta, "windows-x86_64", false, &[])
                .unwrap()
                .is_some()
        );
        let stable_release =
            check(&manifest("0.3.0"), &beta, "windows-x86_64", false, &[]).unwrap();
        assert!(stable_release.is_some());

        // 0.2.0-beta.1 is older than 0.2.0.
        let release = Version::parse("0.2.0").unwrap();
        assert_eq!(
            check(&manifest("0.2.0-beta.1"), &release, "windows-x86_64", true, &[]).unwrap(),
            None
        );
        let early = Version::parse("0.2.0-beta.1").unwrap();
        assert!(check(&manifest("0.2.0"), &early, "windows-x86_64", false, &[]).unwrap().is_some());
    }

    #[test]
    fn check_missing_platform_asset() {
        let current = Version::parse("0.1.0").unwrap();
        let m = manifest("0.2.0");
        assert_eq!(check(&m, &current, "linux-x86_64", false, &[]).unwrap(), None);
    }

    #[test]
    fn check_skipped_version() {
        let current = Version::parse("0.1.0").unwrap();
        let m = manifest("0.2.0");
        let skipped = vec!["v0.2.0".to_string()];
        assert_eq!(check(&m, &current, "windows-x86_64", false, &skipped).unwrap(), None);

        // Only the skipped version is hidden; a later one is still offered.
        let later = manifest("0.2.1");
        assert!(check(&later, &current, "windows-x86_64", false, &skipped).unwrap().is_some());
    }

    #[test]
    fn check_passes_notes_through() {
        let json = r#"{"version": "0.2.0", "notes": "Hello", "assets": {"windows-x86_64": {"url": "https://e.com/a.msi", "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}}}"#;
        let m = parse_manifest(json).unwrap();
        let current = Version::parse("0.1.0").unwrap();
        let available = check(&m, &current, "windows-x86_64", false, &[]).unwrap().unwrap();
        assert_eq!(available.notes.as_deref(), Some("Hello"));
    }

    #[test]
    #[cfg(all(windows, target_arch = "x86_64"))]
    fn platform_key_on_windows() {
        assert_eq!(platform_key(), "windows-x86_64");
    }

    #[test]
    #[cfg(windows)]
    fn install_command_quotes_paths() {
        let (program, args) = install_command(
            Path::new(r"C:\Users\Jo Doe\Downloads\vdm 0.2.0.msi"),
            Path::new(r"C:\Users\Jo Doe\AppData\Local\Temp\vdm update.log"),
            Path::new(r"C:\Program Files\VDM\vdm-gui.exe"),
        )
        .unwrap();
        assert_eq!(program, "cmd");
        assert_eq!(
            args,
            vec![
                "/d".to_string(),
                "/c".to_string(),
                r#""ping -n 3 127.0.0.1 >nul & msiexec /i "C:\Users\Jo Doe\Downloads\vdm 0.2.0.msi" /qn /norestart /l*v "C:\Users\Jo Doe\AppData\Local\Temp\vdm update.log" && start "" "C:\Program Files\VDM\vdm-gui.exe"""#
                    .to_string(),
            ],
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn install_command_none_elsewhere() {
        let cmd = install_command(
            Path::new("/tmp/a.msi"),
            Path::new("/tmp/a.log"),
            Path::new("/usr/bin/vdm"),
        );
        assert_eq!(cmd, None);
    }

    #[test]
    fn current_version_parses() {
        assert_eq!(current_version().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
