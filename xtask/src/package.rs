//! `cargo xtask package`: the Linux tarball and the macOS disk image.
//!
//! Linux: `VibeDownloadManager-<version>-linux-<arch>.tar.gz` holding the
//! binaries, a desktop entry, icons and install/uninstall scripts that set
//! things up for the current user (no root).
//!
//! macOS: `VibeDownloadManager-<version>-macos-<arch>.dmg` holding
//! "Vibe Download Manager.app" (universal when several targets are given),
//! ad-hoc signed, next to a link to /Applications.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::dist::{run_cmd, workspace_root, workspace_version};
use crate::icons::icon_png;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Use the binaries already built.
    #[arg(long)]
    skip_build: bool,
    /// Output folder.
    #[arg(long, default_value = "dist")]
    out: PathBuf,
    /// Rust targets to build (macOS: several are combined into universal
    /// binaries). Default: this machine's.
    #[arg(long)]
    target: Vec<String>,
}

/// The executables every package ships.
const EXES: [&str; 3] = ["vdm-gui", "vdm", "vdm-nmhost"];
const APP_NAME: &str = "Vibe Download Manager";
/// Matches the macOS start-at-sign-in LaunchAgent label.
const BUNDLE_ID: &str = "com.vdm.gui";
const LINUX_ID: &str = "vibe-download-manager";

pub fn run(args: Args) -> Result<()> {
    let root = workspace_root();
    let version = workspace_version(&root)?;
    let out = root.join(&args.out);
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;

    let bins = binary_dirs(&root, &args.target);
    if !args.skip_build {
        build(&root, &args.target)?;
    }
    for dir in &bins {
        for exe in EXES {
            let path = dir.join(exe);
            if !path.is_file() {
                bail!("{} is missing; build first or drop --skip-build", path.display());
            }
        }
    }

    let file = match std::env::consts::OS {
        "linux" => linux(&root, &version, &bins[0], &out)?,
        "macos" => macos(&root, &version, &bins, &out)?,
        other => {
            bail!("`package` makes Linux and macOS packages, not {other}; use `dist` on Windows")
        }
    };
    println!("built {}", file.display());
    Ok(())
}

fn binary_dirs(root: &Path, targets: &[String]) -> Vec<PathBuf> {
    let target = root.join("target");
    if targets.is_empty() {
        vec![target.join("release")]
    } else {
        targets.iter().map(|t| target.join(t).join("release")).collect()
    }
}

fn build(root: &Path, targets: &[String]) -> Result<()> {
    let packages = ["-p", "vdm-gui", "-p", "vdm-cli", "-p", "vdm-nmhost"];
    if targets.is_empty() {
        return run_cmd(
            Command::new(env!("CARGO"))
                .current_dir(root)
                .args(["build", "--release"])
                .args(packages),
        );
    }
    for target in targets {
        run_cmd(
            Command::new(env!("CARGO"))
                .current_dir(root)
                .args(["build", "--release", "--target", target])
                .args(packages),
        )?;
    }
    Ok(())
}

// ------------------------------------------------------------------ Linux

fn linux(root: &Path, version: &str, bin: &Path, out: &Path) -> Result<PathBuf> {
    let name = format!("VibeDownloadManager-{version}-linux-{}", std::env::consts::ARCH);
    let parent = out.join("stage-linux");
    let stage = parent.join(&name);
    fresh_dir(&stage)?;

    for exe in EXES {
        let to = stage.join("bin").join(exe);
        copy(&bin.join(exe), &to)?;
        make_executable(&to)?;
    }
    write(&stage.join("share/applications").join(format!("{LINUX_ID}.desktop")), desktop_entry())?;
    for size in [256, 512] {
        let dir = stage.join(format!("share/icons/hicolor/{size}x{size}/apps"));
        write(&dir.join(format!("{LINUX_ID}.png")), icon_png(size)?)?;
    }
    for (file, text) in [("install.sh", install_script()), ("uninstall.sh", uninstall_script())] {
        write(&stage.join(file), text)?;
        make_executable(&stage.join(file))?;
    }
    write(&stage.join("README.txt"), linux_readme(version))?;
    copy_licenses(root, &stage)?;

    let file = out.join(format!("{name}.tar.gz"));
    run_cmd(Command::new("tar").arg("-C").arg(&parent).arg("-czf").arg(&file).arg(&name))?;
    Ok(file)
}

/// The applications-menu entry. `@DIR@` becomes the install folder.
fn desktop_entry() -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={APP_NAME}\n\
         GenericName=Download Manager\n\
         Comment=Fast downloads that resume where they left off\n\
         Exec=\"@DIR@/vdm-gui\" %U\n\
         Icon={LINUX_ID}\n\
         Terminal=false\n\
         Categories=Network;FileTransfer;\n\
         Keywords=download;accelerator;resume;video;\n"
    )
}

fn install_script() -> String {
    format!(
        r#"#!/bin/sh
# Installs {APP_NAME} for the current user; no root needed.
# To remove it, run ~/.local/lib/{LINUX_ID}/uninstall.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
data="${{XDG_DATA_HOME:-$HOME/.local/share}}"
dest="$HOME/.local/lib/{LINUX_ID}"
bin="$HOME/.local/bin"

mkdir -p "$dest" "$bin" "$data/applications"
cp "$here/bin/vdm-gui" "$here/bin/vdm" "$here/bin/vdm-nmhost" "$here/uninstall.sh" "$dest/"
ln -sf "$dest/vdm-gui" "$bin/vdm-gui"
ln -sf "$dest/vdm" "$bin/vdm"
for size in 256 512; do
  icons="$data/icons/hicolor/${{size}}x${{size}}/apps"
  mkdir -p "$icons"
  cp "$here/share/icons/hicolor/${{size}}x${{size}}/apps/{LINUX_ID}.png" "$icons/"
done
sed "s|@DIR@|$dest|g" "$here/share/applications/{LINUX_ID}.desktop" >"$data/applications/{LINUX_ID}.desktop"
if command -v update-desktop-database >/dev/null 2>&1; then update-desktop-database -q "$data/applications" || true; fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then gtk-update-icon-cache -q -t "$data/icons/hicolor" || true; fi

# Lets the browser extension reach Vibe.
if ! "$dest/vdm-nmhost" install >/dev/null; then
  echo "Note: the browser connection was not set up; run: $dest/vdm-nmhost install" >&2
fi

echo "{APP_NAME} is installed. Open it from your applications menu, or run: vdm-gui"
case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "To use the vdm command, add $bin to your PATH." ;;
esac
"#
    )
}

fn uninstall_script() -> String {
    format!(
        r#"#!/bin/sh
# Removes {APP_NAME}. Your downloads, list and settings are kept.
set -eu
data="${{XDG_DATA_HOME:-$HOME/.local/share}}"
config="${{XDG_CONFIG_HOME:-$HOME/.config}}"
dest="$HOME/.local/lib/{LINUX_ID}"
bin="$HOME/.local/bin"

if [ -x "$dest/vdm-nmhost" ]; then "$dest/vdm-nmhost" uninstall >/dev/null || true; fi
rm -f "$bin/vdm-gui" "$bin/vdm"
rm -f "$data/applications/{LINUX_ID}.desktop" "$config/autostart/{LINUX_ID}.desktop"
rm -f "$data/icons/hicolor/256x256/apps/{LINUX_ID}.png" "$data/icons/hicolor/512x512/apps/{LINUX_ID}.png"
rm -rf "$dest"
echo "{APP_NAME} is removed. Your downloads and settings are kept."
"#
    )
}

fn linux_readme(version: &str) -> String {
    format!(
        "{APP_NAME} {version} for Linux\n\
         \n\
         Install for your user (no root):   ./install.sh\n\
         Remove it again:                    ~/.local/lib/{LINUX_ID}/uninstall.sh\n\
         \n\
         The app opens from your applications menu or with `vdm-gui`; the command-line\n\
         client is `vdm`. Both go to ~/.local/bin.\n\
         \n\
         There is no tray icon on Linux yet, and the app does not update itself:\n\
         get new versions from https://github.com/xMuratY/vdm/releases\n\
         \n\
         The browser extension is a separate download on the same page.\n"
    )
}

// ------------------------------------------------------------------ macOS

fn macos(root: &Path, version: &str, bins: &[PathBuf], out: &Path) -> Result<PathBuf> {
    let arch = if bins.len() > 1 { "universal" } else { std::env::consts::ARCH };
    let parent = out.join("stage-macos");
    fresh_dir(&parent)?;
    let image = parent.join("image");
    let app = image.join(format!("{APP_NAME}.app"));
    let contents = app.join("Contents");

    for exe in EXES {
        let to = contents.join("MacOS").join(exe);
        std::fs::create_dir_all(to.parent().context("no parent")?)?;
        if bins.len() > 1 {
            let mut lipo = Command::new("lipo");
            lipo.arg("-create").arg("-output").arg(&to);
            for dir in bins {
                lipo.arg(dir.join(exe));
            }
            run_cmd(&mut lipo)?;
        } else {
            copy(&bins[0].join(exe), &to)?;
        }
        make_executable(&to)?;
    }
    write(&contents.join("Info.plist"), info_plist(version))?;

    let iconset = parent.join("vdm.iconset");
    std::fs::create_dir_all(&iconset)?;
    for (file, size) in ICONSET {
        write(&iconset.join(file), icon_png(size)?)?;
    }
    let resources = contents.join("Resources");
    std::fs::create_dir_all(&resources)?;
    run_cmd(
        Command::new("iconutil")
            .args(["-c", "icns", "-o"])
            .arg(resources.join("vdm.icns"))
            .arg(&iconset),
    )?;

    // Ad-hoc: no Apple certificate, but a valid signature keeps macOS from
    // calling the app damaged.
    run_cmd(Command::new("codesign").args(["--force", "--deep", "--sign", "-"]).arg(&app))?;

    write(&image.join("Read me.txt"), macos_readme(version))?;
    symlink(Path::new("/Applications"), &image.join("Applications"))?;
    copy_licenses(root, &image)?;

    let file = out.join(format!("VibeDownloadManager-{version}-macos-{arch}.dmg"));
    run_cmd(
        Command::new("hdiutil")
            .args(["create", "-volname", APP_NAME, "-fs", "HFS+", "-format", "UDZO", "-ov"])
            .arg("-srcfolder")
            .arg(&image)
            .arg(&file),
    )?;
    Ok(file)
}

/// The files `iconutil` expects in an iconset, with their pixel sizes.
const ICONSET: [(&str, u32); 10] = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
];

fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>{APP_NAME}</string>
  <key>CFBundleExecutable</key><string>vdm-gui</string>
  <key>CFBundleIconFile</key><string>vdm</string>
  <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>{APP_NAME}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>MIT OR Apache-2.0</string>
</dict>
</plist>
"#
    )
}

fn macos_readme(version: &str) -> String {
    let app = format!("/Applications/{APP_NAME}.app");
    format!(
        "{APP_NAME} {version} for macOS\n\
         \n\
         Drag \"{APP_NAME}\" onto Applications.\n\
         \n\
         The app isn't notarized by Apple, so the first time macOS refuses to open it.\n\
         Open System Settings > Privacy & Security, scroll down and click \"Open Anyway\"\n\
         next to {APP_NAME}. Or run once in Terminal:\n\
         \n\
         \x20   xattr -dr com.apple.quarantine \"{app}\"\n\
         \n\
         Command-line client:   \"{app}/Contents/MacOS/vdm\"\n\
         Browser extension:     run once  \"{app}/Contents/MacOS/vdm-nmhost\" install\n\
         \x20                      then load the extension (a separate download).\n\
         \n\
         The app does not update itself on macOS yet: get new versions from\n\
         https://github.com/xMuratY/vdm/releases\n"
    )
}

// ---------------------------------------------------------------- helpers

/// Delete `dir` if present and create it empty.
fn fresh_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("clearing {}", dir.display()))?;
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn copy(from: &Path, to: &Path) -> Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::copy(from, to)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    Ok(())
}

fn copy_licenses(root: &Path, to: &Path) -> Result<()> {
    for file in ["LICENSE-MIT", "LICENSE-APACHE"] {
        copy(&root.join(file), &to.join(file))?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).with_context(|| format!("linking {}", link.display()))
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> Result<()> {
    bail!("symlinks are only made on macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_points_at_the_install_folder() {
        let entry = desktop_entry();
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("\nExec=\"@DIR@/vdm-gui\" %U\n"));
        assert!(entry.contains("\nIcon=vibe-download-manager\n"));
        assert!(entry.lines().all(|l| l.is_empty() || l.starts_with('[') || l.contains('=')));
    }

    #[test]
    fn scripts_are_posix_sh_and_agree_on_paths() {
        let (install, uninstall) = (install_script(), uninstall_script());
        for script in [&install, &uninstall] {
            assert!(script.starts_with("#!/bin/sh\n"));
            assert!(script.contains("set -eu"));
            assert!(script.contains(r#"dest="$HOME/.local/lib/vibe-download-manager""#));
            assert!(!script.contains("{{") && !script.contains("}}"), "format escapes left");
        }
        assert!(install.contains(r#""$dest/vdm-nmhost" install"#));
        assert!(uninstall.contains(r#""$dest/vdm-nmhost" uninstall"#));
        // The uninstall script is copied next to the app, where its message says it is.
        assert!(install.contains(r#""$here/uninstall.sh" "$dest/""#));
        // The autostart entry the app writes is removed too.
        assert!(uninstall.contains("autostart/vibe-download-manager.desktop"));
        assert!(install.contains(r#"${XDG_DATA_HOME:-$HOME/.local/share}"#));
    }

    #[test]
    fn info_plist_names_the_executable_and_icon() {
        let plist = info_plist("1.2.3");
        assert!(plist.contains("<key>CFBundleExecutable</key><string>vdm-gui</string>"));
        assert!(plist.contains("<key>CFBundleIconFile</key><string>vdm</string>"));
        assert!(plist.contains("<key>CFBundleShortVersionString</key><string>1.2.3</string>"));
        assert_eq!(plist.matches("<key>").count(), plist.matches("</key>").count());
        assert!(plist.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn iconset_has_every_size_iconutil_wants() {
        for (file, size) in ICONSET {
            let (base, scale) = match file.strip_suffix("@2x.png") {
                Some(base) => (base, 2),
                None => (file.strip_suffix(".png").unwrap(), 1),
            };
            let points: u32 = base.rsplit('x').next().unwrap().parse().unwrap();
            assert_eq!(points * scale, size, "{file}");
        }
    }

    #[test]
    fn readmes_mention_how_to_start() {
        assert!(linux_readme("1.0.0").contains("./install.sh"));
        let mac = macos_readme("1.0.0");
        assert!(mac.contains("Open Anyway") && mac.contains("xattr -dr com.apple.quarantine"));
        assert!(mac.contains("    xattr"), "the command is indented: {mac}");
    }
}
