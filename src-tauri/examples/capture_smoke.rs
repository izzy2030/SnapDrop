//! Smoke test for the capture pipeline: grabs the primary monitor's top-left
//! quadrant, encodes a PNG, and writes it to the temp dir.
//! Run with: cargo run --example capture_smoke
//! (Requires a real interactive desktop; the screen dims? No — this example
//! only captures, it does not show the overlay.)

use std::env;
use std::fs;
use std::path::PathBuf;

use windows::Win32::Foundation::RECT;

fn main() {
    // Enumerate monitors.
    let monitors = snapdrop_lib::test_helpers::enumerate_monitors();
    println!("monitors: {}", monitors.len());
    for m in &monitors {
        println!(
            "  rect=({},{},{},{}) scale={}",
            m.rect.left, m.rect.top, m.rect.right, m.rect.bottom, m.scale
        );
    }
    let primary = monitors.iter().find(|m| m.is_primary).or_else(|| monitors.first());
    let Some(primary) = primary else {
        println!("no monitors found");
        std::process::exit(1);
    };

    // Capture the full primary monitor.
    let full = snapdrop_lib::test_helpers::capture_monitor(primary);
    match full {
        Some(img) => println!(
            "captured primary monitor: {}x{} ({} bytes)",
            img.width,
            img.height,
            img.bgra.len()
        ),
        None => {
            println!("CAPTURE FAILED");
            std::process::exit(1);
        }
    }

    // Capture a region: the top-left quadrant.
    let w = (primary.rect.right - primary.rect.left) / 2;
    let h = (primary.rect.bottom - primary.rect.top) / 2;
    let sel = RECT {
        left: primary.rect.left,
        top: primary.rect.top,
        right: primary.rect.left + w,
        bottom: primary.rect.top + h,
    };
    let img = snapdrop_lib::test_helpers::capture_region(&sel, &monitors);
    let img = match img {
        Some(i) => i,
        None => {
            println!("REGION CAPTURE FAILED");
            std::process::exit(1);
        }
    };

    let png = match snapdrop_lib::test_helpers::encode_png(&img.bgra, img.width, img.height) {
        Ok(p) => p,
        Err(e) => {
            println!("PNG ENCODE FAILED: {e}");
            std::process::exit(1);
        }
    };

    let dir: PathBuf = env::temp_dir().join("snapdrop_smoke");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("capture_smoke.png");
    fs::write(&path, &png).unwrap();
    println!("wrote {} ({} bytes, {}x{})", path.display(), png.len(), img.width, img.height);
    println!("SMOKE OK");
}
