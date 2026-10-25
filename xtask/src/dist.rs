//! `cargo xtask dist`: release binaries → (signed) MSI → `latest.json`.
//!
//! Signing is optional and configured by environment variables, so the same
//! command works locally (unsigned) and in CI (signed):
//! - `VDM_SIGN_PFX` + `VDM_SIGN_PASSWORD`: a PFX certificate file, or
//! - `VDM_SIGN_THUMBPRINT`: a certificate in the Windows certificate store;
//! - `VDM_SIGN_TIMESTAMP`: RFC 3161 timestamp server (default DigiCert's);
//! - `VDM_SIGNTOOL`: path to signtool.exe (default: PATH, then the Windows SDK).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// The extension id the native host allows (fixed by the manifest key).
const CHROME_EXTENSION_ID: &str = "gepekebdmobfjhpghgflnhgbbmjcmhon";
const FIREFOX_EXTENSION_ID: &str = "vdm@vdm.local";
const DEFAULT_TIMESTAMP: &str = "http://timestamp.digicert.com";

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Where release assets are published; `latest.json` points the update
    /// check at `<base-url>/<installer file name>`.
    #[arg(long, default_value = "https://github.com/OWNER/REPO/releases/latest/download")]
    base_url: String,
    /// Release notes for `latest.json`.
    #[arg(long, default_value = "")]
    notes: String,
    /// Use the binaries already in target/release.
    #[arg(long)]
    skip_build: bool,
    /// Output folder.
    #[arg(long, default_value = "dist")]
    out: PathBuf,
}

pub fn run(args: Args) -> Result<()> {
    let root = workspace_root();
    let version = workspace_version(&root)?;
    let out = root.join(&args.out);
    let stage = out.join("stage");
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;

    if !args.skip_build {
        run_cmd(Command::new(env!("CARGO")).current_dir(&root).args([
            "build",
            "--release",
            "-p",
            "vdm-gui",
            "-p",
            "vdm-cli",
            "-p",
            "vdm-nmhost",
        ]))?;
    }
    let bin = root.join("target").join("release");
    let exes = ["vdm-gui.exe", "vdm.exe", "vdm-nmhost.exe"].map(|e| bin.join(e));
    for exe in &exes {
        if !exe.is_file() {
            bail!("{} is missing; build first or drop --skip-build", exe.display());
        }
    }

    write_host_manifests(&stage)?;

    let signer = Signer::from_env()?;
    match &signer {
        Some(s) => {
            println!("signing with {}", s.describe());
            for exe in &exes {
                s.sign(exe)?;
            }
        }
        None => println!(
            "no signing certificate configured (VDM_SIGN_PFX / VDM_SIGN_THUMBPRINT); building unsigned"
        ),
    }

    let arch = std::env::consts::ARCH;
    let msi_name = format!("VibeDownloadManager-{version}-{arch}.msi");
    let msi = out.join(&msi_name);
    // The installer's pages and its "launch when done" action come from these
    // extensions; `extension add` fetches them once into `.wix/`.
    run_cmd(
        Command::new("dotnet")
            .current_dir(&root)
            .args(["tool", "run", "wix", "extension", "add"])
            .args(WIX_EXTENSIONS),
    )?;
    run_cmd(
        Command::new("dotnet")
            .current_dir(&root)
            .args(["tool", "run", "wix", "build", "installer/vdm.wxs", "-arch", wix_arch(arch)?])
            .args(WIX_EXTENSIONS.iter().flat_map(|e| ["-ext", e.split('/').next().unwrap_or(e)]))
            .arg("-d")
            .arg(format!("Version={version}"))
            .arg("-d")
            .arg(format!("BinDir={}", bin.display()))
            .arg("-d")
            .arg(format!("StageDir={}", stage.display()))
            .arg("-d")
            .arg(format!("IconPath={}", root.join("assets/icons/vdm.ico").display()))
            .arg("-o")
            .arg(&msi),
    )?;
    if let Some(s) = &signer {
        s.sign(&msi)?;
    }

    let bytes = std::fs::read(&msi).with_context(|| format!("reading {}", msi.display()))?;
    let sha256 = hex(&Sha256::digest(&bytes));
    let manifest = update_manifest(
        &version,
        &args.base_url,
        &msi_name,
        &sha256,
        bytes.len() as u64,
        &args.notes,
    );
    let manifest_path = out.join("latest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)? + "\n")?;

    println!("built {} ({} bytes, sha256 {sha256})", msi.display(), bytes.len());
    println!("wrote {}", manifest_path.display());
    Ok(())
}

/// `latest.json`, as read by the app's update check.
fn update_manifest(
    version: &str,
    base_url: &str,
    file: &str,
    sha256: &str,
    size: u64,
    notes: &str,
) -> serde_json::Value {
    let key = format!("windows-{}", std::env::consts::ARCH);
    let mut manifest = serde_json::json!({
        "version": version,
        "assets": {
            key: {
                "url": format!("{}/{file}", base_url.trim_end_matches('/')),
                "sha256": sha256,
                "size": size,
            }
        }
    });
    if !notes.trim().is_empty() {
        manifest["notes"] = notes.trim().into();
    }
    manifest
}

/// Native-messaging manifests installed next to vdm-nmhost.exe. The relative
/// "path" is resolved against the manifest's folder on Windows.
fn write_host_manifests(stage: &Path) -> Result<()> {
    let base = serde_json::json!({
        "name": "com.vdm.native",
        "description": "Vibe Download Manager",
        "path": "vdm-nmhost.exe",
        "type": "stdio",
    });
    let mut chrome = base.clone();
    chrome["allowed_origins"] =
        serde_json::json!([format!("chrome-extension://{CHROME_EXTENSION_ID}/")]);
    let mut firefox = base;
    firefox["allowed_extensions"] = serde_json::json!([FIREFOX_EXTENSION_ID]);
    for (name, value) in [("chrome", chrome), ("firefox", firefox)] {
        let path = stage.join(format!("com.vdm.native.{name}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&value)? + "\n")
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// How to sign, from the environment.
enum Signer {
    Pfx { signtool: PathBuf, pfx: PathBuf, password: String, timestamp: String },
    Store { signtool: PathBuf, thumbprint: String, timestamp: String },
}

impl Signer {
    fn from_env() -> Result<Option<Self>> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let timestamp = var("VDM_SIGN_TIMESTAMP").unwrap_or_else(|| DEFAULT_TIMESTAMP.into());
        if let Some(pfx) = var("VDM_SIGN_PFX") {
            let password = var("VDM_SIGN_PASSWORD").unwrap_or_default();
            return Ok(Some(Self::Pfx {
                signtool: signtool()?,
                pfx: pfx.into(),
                password,
                timestamp,
            }));
        }
        if let Some(thumbprint) = var("VDM_SIGN_THUMBPRINT") {
            return Ok(Some(Self::Store { signtool: signtool()?, thumbprint, timestamp }));
        }
        Ok(None)
    }

    fn describe(&self) -> String {
        match self {
            Self::Pfx { pfx, .. } => format!("certificate file {}", pfx.display()),
            Self::Store { thumbprint, .. } => {
                format!("certificate {thumbprint} from the certificate store")
            }
        }
    }

    fn sign(&self, file: &Path) -> Result<()> {
        let (signtool, timestamp) = match self {
            Self::Pfx { signtool, timestamp, .. } | Self::Store { signtool, timestamp, .. } => {
                (signtool, timestamp)
            }
        };
        let mut cmd = Command::new(signtool);
        cmd.args(["sign", "/fd", "sha256", "/td", "sha256", "/d", "Vibe Download Manager"]);
        // An empty timestamp server means "don't timestamp" (offline test builds).
        if !timestamp.eq_ignore_ascii_case("none") {
            cmd.args(["/tr", timestamp]);
        }
        match self {
            Self::Pfx { pfx, password, .. } => {
                cmd.arg("/f").arg(pfx);
                if !password.is_empty() {
                    cmd.args(["/p", password]);
                }
            }
            Self::Store { thumbprint, .. } => {
                cmd.args(["/sha1", thumbprint]);
            }
        }
        cmd.arg(file);
        run_cmd(&mut cmd).with_context(|| format!("signing {}", file.display()))
    }
}

/// signtool.exe from VDM_SIGNTOOL, PATH, or the newest Windows SDK.
fn signtool() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("VDM_SIGNTOOL") {
        return Ok(path.into());
    }
    if let Some(found) = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p).map(|d| d.join("signtool.exe")).find(|p| p.is_file())
    }) {
        return Ok(found);
    }
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };
    let mut versions: Vec<PathBuf> = std::fs::read_dir(kits)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|v| v.join(arch).join("signtool.exe"))
        .find(|p| p.is_file())
        .context("signtool.exe not found; install the Windows SDK or set VDM_SIGNTOOL")
}

/// WiX extensions the installer uses, pinned to the WiX version.
const WIX_EXTENSIONS: [&str; 2] = ["WixToolset.UI.wixext/5.0.2", "WixToolset.Util.wixext/5.0.2"];

fn wix_arch(arch: &str) -> Result<&'static str> {
    Ok(match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        other => bail!("no installer for architecture {other}"),
    })
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask is in the workspace").to_owned()
}

/// `version = "…"` from the workspace Cargo.toml.
fn workspace_version(root: &Path) -> Result<String> {
    let text = std::fs::read_to_string(root.join("Cargo.toml"))?;
    let in_package = text.split("[workspace.package]").nth(1).context("no [workspace.package]")?;
    in_package
        .lines()
        .find_map(|l| l.trim().strip_prefix("version").map(|r| r.trim_start_matches([' ', '='])))
        .map(|v| v.trim().trim_matches('"').to_owned())
        .context("no workspace version")
}

fn run_cmd(cmd: &mut Command) -> Result<()> {
    let shown = format!("{cmd:?}");
    let status = cmd.status().with_context(|| format!("could not run {shown}"))?;
    if !status.success() {
        bail!("{shown} failed with {status}");
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_shape() {
        let m =
            update_manifest("0.2.0", "https://x/dl/", "vdm-0.2.0-x86_64.msi", "ab", 5, " notes ");
        let key = format!("windows-{}", std::env::consts::ARCH);
        assert_eq!(m["version"], "0.2.0");
        assert_eq!(m["assets"][&key]["url"], "https://x/dl/vdm-0.2.0-x86_64.msi");
        assert_eq!(m["assets"][&key]["sha256"], "ab");
        assert_eq!(m["assets"][&key]["size"], 5);
        assert_eq!(m["notes"], "notes");
        assert!(update_manifest("1.0.0", "u", "f", "h", 1, "").get("notes").is_none());
    }

    #[test]
    fn version_from_this_workspace() {
        let v = workspace_version(&workspace_root()).unwrap();
        assert_eq!(v, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn host_manifests_use_relative_paths() {
        let dir = std::env::temp_dir().join(format!("vdm-xtask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_host_manifests(&dir).unwrap();
        let chrome: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("com.vdm.native.chrome.json")).unwrap())
                .unwrap();
        assert_eq!(chrome["path"], "vdm-nmhost.exe");
        assert_eq!(
            chrome["allowed_origins"][0],
            "chrome-extension://gepekebdmobfjhpghgflnhgbbmjcmhon/"
        );
        let firefox: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join("com.vdm.native.firefox.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(firefox["allowed_extensions"][0], "vdm@vdm.local");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
