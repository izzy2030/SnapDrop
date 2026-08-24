//! Screenshot file naming, PNG encoding, and directory helpers.

use std::env;
use std::fs;
use std::path::PathBuf;

use chrono::Local;
use image::codecs::png::PngEncoder;
use image::ImageEncoder;

/// Default capture directory: %USERPROFILE%\Pictures\SnapDrop
pub fn default_screenshot_dir() -> PathBuf {
    match env::var("USERPROFILE") {
        Ok(p) => PathBuf::from(p).join("Pictures").join("SnapDrop"),
        Err(_) => PathBuf::from("Pictures").join("SnapDrop"),
    }
}

/// Expand %USERPROFILE% in a configured path.
pub fn expand_dir(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return default_screenshot_dir();
    }
    if trimmed.contains("%USERPROFILE%") {
        if let Ok(p) = env::var("USERPROFILE") {
            return PathBuf::from(trimmed.replace("%USERPROFILE%", &p));
        }
    }
    PathBuf::from(trimmed)
}

/// Validate that a directory exists (or can be created) and is writable.
pub fn ensure_dir_writable(dir: &PathBuf) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("Cannot create folder {}: {e}", dir.display()))?;
    let probe = dir.join(format!(".snapdrop_write_test_{}", std::process::id()));
    fs::write(&probe, b"ok").map_err(|e| format!("Folder {} is not writable: {e}", dir.display()))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

/// Next unique filename: SnapDrop_YYYY-MM-DD_HHMMSS.png with _1/_2 collision suffixes.
pub fn next_filename(dir: &PathBuf) -> PathBuf {
    let stamp = Local::now().format("%Y-%m-%d_%H%M%S").to_string();
    let mut n: u32 = 0;
    loop {
        let name = if n == 0 {
            format!("SnapDrop_{stamp}.png")
        } else {
            format!("SnapDrop_{stamp}_{n}.png")
        };
        let candidate = dir.join(&name);
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Encode a BGRA buffer (alpha ignored) into PNG bytes.
pub fn encode_png(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, image::ImageError> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    let mut out = Vec::new();
    PngEncoder::new(&mut out).write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)?;
    Ok(out)
}

/// Downscale a BGRA buffer to fit within `max_dim` and encode as a small PNG (for previews).
pub fn preview_png(bgra: &[u8], width: u32, height: u32, max_dim: u32) -> Option<Vec<u8>> {
    use image::imageops::thumbnail;
    use image::ImageBuffer;

    let mut rgba = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    let img = ImageBuffer::<image::Rgba<u8>, Vec<u8>>::from_raw(width, height, rgba)?;
    let longest = width.max(height);
    let (tw, th) = if longest <= max_dim {
        (width, height)
    } else {
        let scale = max_dim as f64 / longest as f64;
        (
            ((width as f64 * scale).round() as u32).max(1),
            ((height as f64 * scale).round() as u32).max(1),
        )
    };
    let thumb = thumbnail(&img, tw, th);
    let (actual_tw, actual_th) = thumb.dimensions();
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(
            thumb.as_raw(),
            actual_tw,
            actual_th,
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_filename_collision_suffix() {
        let dir = env::temp_dir().join(format!("snapdrop_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Create a fake existing file matching the current timestamp pattern.
        let stamp = Local::now().format("%Y-%m-%d_%H%M%S").to_string();
        let existing = dir.join(format!("SnapDrop_{stamp}.png"));
        fs::write(&existing, b"x").unwrap();
        let next = next_filename(&dir);
        let name = next.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with(&format!("SnapDrop_{stamp}_")), "got {name}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_dir_replaces_userprofile() {
        if let Ok(p) = env::var("USERPROFILE") {
            let d = expand_dir("%USERPROFILE%\\Pictures\\SnapDrop");
            assert!(d.starts_with(PathBuf::from(p)));
        }
    }

    #[test]
    fn encode_png_roundtrip() {
        let w = 3u32;
        let h = 2u32;
        let bgra: Vec<u8> = (0..(w * h) as usize)
            .flat_map(|i| {
                let v = (i * 40 % 256) as u8;
                [v, 128, 255 - v, 0]
            })
            .collect();
        let png = encode_png(&bgra, w, h).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn preview_png_1171x964_no_panic() {
        use image::GenericImageView;
        // Regression test for thumbnail buffer-size panic: 1171×964 with max_dim 512
        // previously calculated tw=512, th=421 but `thumbnail()` returned 511×421,
        // causing a panic when encoding with the requested size.
        let w = 1171u32;
        let h = 964u32;
        let bgra = vec![0u8; (w * h * 4) as usize];
        let png = preview_png(&bgra, w, h, 512);
        assert!(png.is_some(), "preview_png should succeed for 1171x964");
        let png = png.unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
        // Also verify the resulting PNG can be decoded and has the expected aspect
        let img = image::load_from_memory(&png).unwrap();
        let (tw, th) = img.dimensions();
        // Should fit within 512×512 and preserve aspect (1171×964 ≈ 1.214, 512×421 ≈ 1.216)
        assert!(tw <= 512 && th <= 512);
        assert!((tw as i32 - 511).abs() <= 1 && (th as i32 - 421).abs() <= 1);
    }
}
