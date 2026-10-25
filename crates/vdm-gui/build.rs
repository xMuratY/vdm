//! Build script: on Windows, embeds the app icon and version information in
//! the executables. Other targets get nothing.

const ICON: &str = "../../assets/icons/vdm.ico";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_resources();
    }
}

/// Icon and version strings for the Windows resource file. The file and
/// product versions come from the crate version (winresource does that itself).
#[cfg(windows)]
fn embed_windows_resources() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ICON)
        .set("ProductName", "Vibe Download Manager")
        .set("FileDescription", "Vibe Download Manager")
        .set("CompanyName", "Vibe")
        .set("LegalCopyright", "\u{a9} Vibe Download Manager contributors");
    res.compile().expect("could not embed the Windows icon and version info");
}

/// The Windows resource compiler only runs on a Windows host. A non-Windows
/// host cross-compiling to Windows builds without the icon and version info.
#[cfg(not(windows))]
fn embed_windows_resources() {
    println!("cargo:warning=Windows icon and version info are not embedded (not a Windows host)");
}
