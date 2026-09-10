//! Headless end-to-end smoke test of the production recording path.
//!
//! Drives the real `VideoSession` — WGC capture, GPU crop, Media Foundation
//! encode, the audio pump thread, and finalize — with no Tauri app, no window
//! and no GUI interaction. Run:
//!   cargo run --release --example video_smoke -- <secs> <fps> <w> <h>
//!
//! Requires something on screen to be changing, otherwise WGC delivers almost
//! no frames and every number here is meaningless.
//!
//! The recorder's own debug log (health lines, audio-pump byte count,
//! finalize) is written next to the MP4 and echoed at the end.

use windows::Win32::Foundation::RECT;
use windows_capture::monitor::Monitor;

fn main() {
    let mut args = std::env::args().skip(1);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(6);
    let fps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let rw: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1280);
    let rh: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(720);

    let tmp = std::env::temp_dir();
    let out = tmp.join("snapdrop_smoke.mp4");
    let log = tmp.join("snapdrop_smoke.log");
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&log);

    let monitor = match Monitor::primary() {
        Ok(m) => m,
        Err(e) => {
            println!("no primary monitor: {e}");
            return;
        }
    };
    let mw = monitor.width().expect("monitor width") as i32;
    let mh = monitor.height().expect("monitor height") as i32;
    let w = rw.min(mw) & !1;
    let h = rh.min(mh) & !1;
    let left = ((mw - w) / 2) & !1;
    let top = ((mh - h) / 2) & !1;
    let region = RECT { left, top, right: left + w, bottom: top + h };

    println!("monitor {mw}x{mh}  region {w}x{h} at ({left},{top})  {secs}s @ {fps} fps");
    println!("output  {}", out.display());
    println!("log     {}", log.display());
    println!();

    let start = std::time::Instant::now();
    let result = snapdrop_lib::test_helpers::record_headless(
        region,
        monitor,
        out.to_string_lossy().to_string(),
        fps,
        secs,
        Some(log.to_string_lossy().to_string()),
        false, // audio enabled
        Some("1080p".to_string()),
    );
    let wall = start.elapsed().as_secs_f64();

    println!("--- result (wall {wall:.2}s) ---");
    match result {
        Ok(r) => {
            let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            println!("  frames      : {}", r.frames);
            println!("  elapsed_ms  : {}", r.elapsed_ms);
            println!("  dims        : {}x{}", r.width, r.height);
            println!("  file bytes  : {size}");
            println!("  effective   : {:.1} fps", r.frames as f64 / (r.elapsed_ms as f64 / 1000.0).max(0.001));
        }
        Err(e) => println!("  FAILED: {e}"),
    }

    println!();
    println!("--- recorder log ---");
    match std::fs::read_to_string(&log) {
        Ok(s) => print!("{s}"),
        Err(e) => println!("(could not read {}: {e})", log.display()),
    }
}
