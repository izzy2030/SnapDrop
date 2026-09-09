//! Headless A/B benchmark for the two ways to hand a region to the encoder.
//!
//! Captures the primary monitor for a few seconds and crops a centred region
//! out of every frame, twice:
//!
//!   * `gpu` — `send_frame_region`, a GPU-side `CopySubresourceRegion` into a
//!     reused texture. No CPU readback, no flip, no per-frame allocation.
//!   * `cpu` — the original path: `buffer_crop` (fresh staging texture per
//!     frame + blocking `Map`), a full row flip, and `send_frame_buffer`.
//!
//! Prints frames, effective fps, frames shed by backpressure, and mean per-frame
//! handler cost. Run with:
//!   cargo run --release --example region_bench -- <seconds> <region_w> <region_h>

use std::io::{self, Write};
use std::time::{Duration, Instant};

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, VideoEncoder, VideoSettingsBuilder, VideoSettingsSubType,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Gpu,
    Cpu,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Gpu => "gpu-crop",
            Mode::Cpu => "cpu-crop",
        }
    }
}

struct Flags {
    mode: Mode,
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
    deadline: Duration,
    path: String,
}

struct Bench {
    encoder: Option<VideoEncoder>,
    mode: Mode,
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
    start: Instant,
    deadline: Duration,
    frames: u64,
    /// Total time spent inside `on_frame_arrived` — the cost WGC has to wait
    /// through before it will deliver the next frame.
    handler_nanos: u128,
    /// Per-frame handler durations, so we can report the tail. Judder comes
    /// from spikes, not from the mean.
    samples: Vec<u128>,
    flip_buf: Vec<u8>,
    path: String,
}

impl GraphicsCaptureApiHandler for Bench {
    type Flags = Flags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let f = ctx.flags;
        let encoder = VideoEncoder::new(
            VideoSettingsBuilder::new(f.width, f.height)
                .sub_type(VideoSettingsSubType::H264)
                .frame_rate(60)
                .bitrate(12_000_000),
            AudioSettingsBuilder::default().disabled(true),
            ContainerSettingsBuilder::default(),
            &f.path,
        )?;
        Ok(Self {
            encoder: Some(encoder),
            mode: f.mode,
            origin_x: f.origin_x,
            origin_y: f.origin_y,
            width: f.width,
            height: f.height,
            start: Instant::now(),
            deadline: f.deadline,
            frames: 0,
            handler_nanos: 0,
            samples: Vec::new(),
            flip_buf: Vec::new(),
            path: f.path,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let entered = Instant::now();
        let ts = frame.timestamp()?.Duration;
        let enc = self.encoder.as_mut().unwrap();

        match self.mode {
            Mode::Gpu => {
                enc.send_frame_region(frame, self.origin_x, self.origin_y, ts)?;
            }
            Mode::Cpu => {
                // Exact production path: fresh staging texture + blocking Map
                // inside buffer_crop, then a full row flip.
                let buf = frame.buffer_crop(
                    self.origin_x,
                    self.origin_y,
                    self.origin_x + self.width,
                    self.origin_y + self.height,
                )?;
                let h = buf.height();
                let mut no_pad: Vec<u8> = Vec::new();
                let bgra = buf.as_nopadding_buffer(&mut no_pad);
                let row = buf.width() as usize * 4;
                self.flip_buf.resize(bgra.len(), 0);
                for y in 0..h as usize {
                    let src = &bgra[y * row..(y + 1) * row];
                    let dst_row = (h as usize - 1 - y) * row;
                    self.flip_buf[dst_row..dst_row + row].copy_from_slice(src);
                }
                enc.send_frame_buffer(&self.flip_buf, ts)?;
            }
        }
        self.frames += 1;
        let took = entered.elapsed().as_nanos();
        self.handler_nanos += took;
        self.samples.push(took);

        if self.start.elapsed() >= self.deadline {
            let dropped = self.encoder.as_ref().map(VideoEncoder::dropped_frames).unwrap_or(0);
            let elapsed = self.start.elapsed();
            self.encoder.take().unwrap().finish()?;
            let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
            println!();
            println!("--- {}", self.mode.label());
            println!("  frames sent      : {}", self.frames);
            println!("  wall clock       : {:.2}s", elapsed.as_secs_f64());
            println!("  effective fps    : {:.1}", self.frames as f64 / elapsed.as_secs_f64());
            println!("  shed (backpress) : {}", dropped);
            let mut s = self.samples.clone();
            s.sort_unstable();
            let pct = |p: usize| -> f64 {
                if s.is_empty() {
                    return 0.0;
                }
                let idx = ((s.len() - 1) * p) / 100;
                s[idx] as f64 / 1_000_000.0
            };
            println!(
                "  handler ms/frame : mean {:.2} | p50 {:.2} | p95 {:.2} | p99 {:.2} | max {:.2}",
                self.handler_nanos as f64 / self.frames.max(1) as f64 / 1_000_000.0,
                pct(50),
                pct(95),
                pct(99),
                s.last().copied().unwrap_or(0) as f64 / 1_000_000.0,
            );
            println!(
                "  frames over 16ms : {} ({:.1}%)",
                s.iter().filter(|&&n| n > 16_000_000).count(),
                100.0 * s.iter().filter(|&&n| n > 16_000_000).count() as f64 / s.len().max(1) as f64
            );
            println!("  output           : {} bytes", size);
            capture_control.stop();
        } else {
            print!("\r{}: {:.1}s ", self.mode.label(), self.start.elapsed().as_secs_f64());
            io::stdout().flush()?;
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let region_w: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(1280);
    let region_h: u32 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(720);

    let monitor = Monitor::primary().expect("no primary monitor");
    let mw = monitor.width().expect("monitor width");
    let mh = monitor.height().expect("monitor height");
    println!("primary monitor: {mw}x{mh}");

    let w = region_w.min(mw) & !1;
    let h = region_h.min(mh) & !1;
    let ox = ((mw - w) / 2) & !1;
    let oy = ((mh - h) / 2) & !1;
    println!("region: {w}x{h} at ({ox},{oy}), {secs}s per mode");

    let tmp = std::env::temp_dir();
    for mode in [Mode::Cpu, Mode::Gpu] {
        let path = tmp.join(format!("snapdrop_bench_{}.mp4", mode.label())).to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);
        let flags = Flags {
            mode,
            origin_x: ox,
            origin_y: oy,
            width: w,
            height: h,
            deadline: Duration::from_secs(secs),
            path: path.clone(),
        };
        let settings = Settings::new(
            Monitor::primary().expect("no primary monitor"),
            CursorCaptureSettings::Default,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Custom(Duration::from_nanos(1_000_000_000 / 60)),
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            flags,
        );
        println!();
        if let Err(e) = Bench::start(settings) {
            eprintln!("{mode:?} capture failed: {e}");
        }
    }
}
