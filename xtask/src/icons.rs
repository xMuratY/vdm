//! Writes the app icon files: under `assets/icons`, a multi-size ICO for the
//! Windows executables and installer and a 256 px PNG for other uses; and the
//! browser extension's toolbar icons under `extension/icons`; and the
//! installer's banner and welcome images under `assets/installer`.
//!
//! The drawing lives in the vdm-gui crate; it is included here directly so
//! xtask does not depend on the GUI binary.

#[path = "../../crates/vdm-gui/src/icon.rs"]
mod icon;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// Sizes in the ICO, in order.
const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

/// The browser extension's icon sizes.
const EXTENSION_SIZES: [u32; 3] = [16, 48, 128];

/// Size of the standalone PNG.
const PNG_SIZE: u32 = 256;

pub fn run() -> anyhow::Result<()> {
    let dir = assets_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let mut images = Vec::with_capacity(ICO_SIZES.len());
    for size in ICO_SIZES {
        images.push((size, png_bytes(size, &icon::icon_rgba(size))?));
    }
    let ico = dir.join("vdm.ico");
    fs::write(&ico, ico_bytes(&images)).with_context(|| format!("writing {}", ico.display()))?;

    let big = images.iter().find(|(size, _)| *size == PNG_SIZE).context("no 256 px image")?;
    let png = dir.join("vdm-256.png");
    fs::write(&png, &big.1).with_context(|| format!("writing {}", png.display()))?;

    let ext = assets_dir()?.join("../../extension/icons");
    fs::create_dir_all(&ext).with_context(|| format!("creating {}", ext.display()))?;
    for size in EXTENSION_SIZES {
        let file = ext.join(format!("icon{size}.png"));
        fs::write(&file, png_bytes(size, &icon::icon_rgba(size))?)
            .with_context(|| format!("writing {}", file.display()))?;
    }

    let installer = assets_dir()?.join("../installer");
    fs::create_dir_all(&installer).with_context(|| format!("creating {}", installer.display()))?;
    let banner = installer.join("banner.bmp");
    fs::write(&banner, bmp_bytes(BANNER.0, BANNER.1, &banner_rgb()))
        .with_context(|| format!("writing {}", banner.display()))?;
    let dialog = installer.join("dialog.bmp");
    fs::write(&dialog, bmp_bytes(DIALOG.0, DIALOG.1, &dialog_rgb()))
        .with_context(|| format!("writing {}", dialog.display()))?;

    println!(
        "wrote {}, {}, the extension icons and the installer images",
        ico.display(),
        png.display()
    );
    Ok(())
}

/// WiX's banner (top of most pages) and dialog (welcome and finish pages) sizes.
const BANNER: (u32, u32) = (493, 58);
const DIALOG: (u32, u32) = (493, 312);
/// The dialog image's left panel; WiX draws the page text to its right.
const PANEL_WIDTH: u32 = 164;

/// White, with the logo at the right edge (WiX writes the title on the left).
fn banner_rgb() -> Vec<u8> {
    let (w, h) = BANNER;
    let mut rgb = vec![255u8; (w * h * 3) as usize];
    let size = 40;
    draw_icon(&mut rgb, w, w - size - 12, (h - size) / 2, size);
    rgb
}

/// A soft violet-to-pink panel on the left with the logo, white elsewhere.
fn dialog_rgb() -> Vec<u8> {
    let (w, h) = DIALOG;
    let mut rgb = vec![255u8; (w * h * 3) as usize];
    let (top, bottom) = ([237.0, 233.0, 254.0], [252.0, 231.0, 243.0]);
    for y in 0..h {
        let t = y as f32 / (h - 1) as f32;
        let colour: Vec<u8> =
            (0..3).map(|i| (top[i] + (bottom[i] - top[i]) * t).round() as u8).collect();
        for x in 0..PANEL_WIDTH {
            let i = ((y * w + x) * 3) as usize;
            rgb[i..i + 3].copy_from_slice(&colour);
        }
    }
    let size = 96;
    draw_icon(&mut rgb, w, (PANEL_WIDTH - size) / 2, 48, size);
    rgb
}

/// Blend the app icon (`size` px) onto an RGB image at (`left`, `top`).
fn draw_icon(rgb: &mut [u8], width: u32, left: u32, top: u32, size: u32) {
    let icon = icon::icon_rgba(size);
    for y in 0..size {
        for x in 0..size {
            let s = ((y * size + x) * 4) as usize;
            let a = f32::from(icon[s + 3]) / 255.0;
            let d = (((top + y) * width + left + x) * 3) as usize;
            for c in 0..3 {
                let blended = f32::from(icon[s + c]) * a + f32::from(rgb[d + c]) * (1.0 - a);
                rgb[d + c] = blended.round() as u8;
            }
        }
    }
}

/// A 24-bit uncompressed BMP (bottom-up rows, padded to 4 bytes).
fn bmp_bytes(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let row = (width * 3).div_ceil(4) * 4;
    let image = row * height;
    let mut out = Vec::with_capacity((54 + image) as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54 + image).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&0u32.to_le_bytes()); // no compression
    out.extend_from_slice(&image.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes()); // 72 dpi
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..height).rev() {
        let start = out.len();
        for x in 0..width {
            let i = ((y * width + x) * 3) as usize;
            out.extend_from_slice(&[rgb[i + 2], rgb[i + 1], rgb[i]]); // BGR
        }
        out.resize(start + row as usize, 0);
    }
    out
}

/// `assets/icons` in the workspace root (xtask lives one level below it).
fn assets_dir() -> anyhow::Result<PathBuf> {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().context("xtask has no parent directory")?;
    Ok(root.join("assets").join("icons"))
}

/// One RGBA image as a PNG file.
fn png_bytes(size: u32, rgba: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(out)
}

/// An ICO file holding PNG-compressed images, given as (size, PNG bytes).
///
/// Layout: an ICONDIR header, one 16-byte ICONDIRENTRY per image, then the
/// image data. All fields are little-endian. A width or height byte of 0
/// means 256.
fn ico_bytes(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    const HEADER: usize = 6;
    const ENTRY: usize = 16;
    let data_start = HEADER + ENTRY * images.len();
    let total: usize = data_start + images.iter().map(|(_, png)| png.len()).sum::<usize>();

    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    let mut offset = data_start;
    for (size, png) in images {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.push(dim); // width
        out.push(dim); // height
        out.push(0); // colour count (0: not palettised)
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += png.len();
    }
    for (_, png) in images {
        out.extend_from_slice(png);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(b: &[u8], i: usize) -> u16 {
        u16::from_le_bytes([b[i], b[i + 1]])
    }

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
    }

    fn all_images() -> Vec<(u32, Vec<u8>)> {
        ICO_SIZES
            .iter()
            .map(|&size| (size, png_bytes(size, &icon::icon_rgba(size)).unwrap()))
            .collect()
    }

    #[test]
    fn bmp_layout() {
        let bmp = bmp_bytes(3, 2, &[255, 0, 0, 0, 255, 0, 0, 0, 255, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(&bmp[..2], b"BM");
        assert_eq!(u32_at(&bmp, 2) as usize, bmp.len());
        assert_eq!(u32_at(&bmp, 18), 3);
        assert_eq!(bmp.len(), 54 + 12 * 2, "rows padded to 4 bytes");
        // The last row comes first, colours as BGR.
        assert_eq!(&bmp[54..57], &[3, 2, 1]);
        assert_eq!(&bmp[66..69], &[0, 0, 255]);
    }

    #[test]
    fn installer_images_have_the_right_size() {
        assert_eq!(banner_rgb().len(), (BANNER.0 * BANNER.1 * 3) as usize);
        assert_eq!(dialog_rgb().len(), (DIALOG.0 * DIALOG.1 * 3) as usize);
    }

    #[test]
    fn ico_header_reads_back() {
        let images = all_images();
        let ico = ico_bytes(&images);

        assert_eq!(u16_at(&ico, 0), 0, "reserved");
        assert_eq!(u16_at(&ico, 2), 1, "type is icon");
        assert_eq!(u16_at(&ico, 4), 7, "seven images");

        let mut expected_offset = 6 + 16 * images.len();
        for (i, (size, png)) in images.iter().enumerate() {
            let e = 6 + 16 * i;
            let dim = if *size == 256 { 0 } else { *size as u8 };
            assert_eq!(ico[e], dim, "width of entry {i}");
            assert_eq!(ico[e + 1], dim, "height of entry {i}");
            assert_eq!(u16_at(&ico, e + 4), 1, "planes of entry {i}");
            assert_eq!(u16_at(&ico, e + 6), 32, "bits of entry {i}");
            let len = u32_at(&ico, e + 8) as usize;
            let offset = u32_at(&ico, e + 12) as usize;
            assert_eq!(len, png.len(), "length of entry {i}");
            assert_eq!(offset, expected_offset, "offset of entry {i}");
            assert_eq!(&ico[offset..offset + 8], b"\x89PNG\r\n\x1a\n", "PNG data {i}");
            expected_offset += len;
        }
        assert_eq!(expected_offset, ico.len(), "entries cover the whole file");
    }

    #[test]
    fn png_round_trips_its_size() {
        let png = png_bytes(16, &icon::icon_rgba(16)).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert_eq!((info.width, info.height), (16, 16));
    }
}
