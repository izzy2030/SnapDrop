//! Screen capture in physical pixels, per monitor.
//!
//! Image capture uses **Windows Graphics Capture (WGC)** — the same vendored
//! pipeline used for video recording — instead of a synchronous GDI readback.
//! A full-desktop `BitBlt`/`GetDIBits` on the app's thread stalls the shared
//! DWM/display present path and can freeze every open app (see
//! `FREEZE_INVESTIGATION.md`); the async WGC session runs on its own thread and
//! is bounded by a timeout, so a stuck GPU can never wedge the session.
//! The old GDI `capture_monitor` is kept only for the `capture_smoke` example.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::mpsc;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDCW, DeleteDC, DeleteObject,
    GetDIBits, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    SRCCOPY,
};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::{
    debuglog,
    monitors::{self, MonitorInfo},
};

pub struct ImageBuf {
    pub width: u32,
    pub height: u32,
    /// BGRA8, top-down. GDI captures set alpha to 0x00; WGC sets it opaque (255).
    pub bgra: Vec<u8>,
}

pub(crate) fn compatible_capture_settings(
    requested_cursor: CursorCaptureSettings,
    requested_border: DrawBorderSettings,
    requested_interval: MinimumUpdateIntervalSettings,
) -> (
    CursorCaptureSettings,
    DrawBorderSettings,
    MinimumUpdateIntervalSettings,
) {
    let cursor = if requested_cursor == CursorCaptureSettings::Default {
        requested_cursor
    } else {
        match GraphicsCaptureApi::is_cursor_settings_supported() {
            Ok(true) => requested_cursor,
            Ok(false) => {
                debuglog::log("capture: cursor settings unsupported; using default");
                CursorCaptureSettings::Default
            }
            Err(e) => {
                debuglog::log(&format!(
                    "capture: cursor settings support check failed; using default: {e}"
                ));
                CursorCaptureSettings::Default
            }
        }
    };
    let border = if requested_border == DrawBorderSettings::Default {
        requested_border
    } else {
        match GraphicsCaptureApi::is_border_settings_supported() {
            Ok(true) => requested_border,
            Ok(false) => {
                debuglog::log("capture: border settings unsupported; using default");
                DrawBorderSettings::Default
            }
            Err(e) => {
                debuglog::log(&format!(
                    "capture: border settings support check failed; using default: {e}"
                ));
                DrawBorderSettings::Default
            }
        }
    };
    let interval = if requested_interval == MinimumUpdateIntervalSettings::Default {
        requested_interval
    } else {
        match GraphicsCaptureApi::is_minimum_update_interval_supported() {
            Ok(true) => requested_interval,
            Ok(false) => {
                debuglog::log("capture: update interval unsupported; using default");
                MinimumUpdateIntervalSettings::Default
            }
            Err(e) => {
                debuglog::log(&format!(
                    "capture: update interval support check failed; using default: {e}"
                ));
                MinimumUpdateIntervalSettings::Default
            }
        }
    };
    (cursor, border, interval)
}

/// Capture the given virtual-screen rect (physical pixels) by stitching per-monitor buffers.
pub fn capture_region(sel: &RECT, monitors: &[MonitorInfo]) -> Option<ImageBuf> {
    let w = monitors::rect_width(sel);
    let h = monitors::rect_height(sel);
    if w <= 0 || h <= 0 {
        return None;
    }
    let (w, h) = (w as u32, h as u32);
    let mut out = vec![0u8; w as usize * h as usize * 4];

    for mon in monitors {
        let inter = monitors::intersect(sel, &mon.rect);
        if monitors::is_empty(&inter) {
            continue;
        }
        let full = capture_monitor_wgc(mon)?;
        // WGC surfaces exactly match the monitor's physical resolution; if a
        // display changed size mid-capture, bail rather than stitch garbage.
        if full.width != monitors::rect_width(&mon.rect) as u32
            || full.height != monitors::rect_height(&mon.rect) as u32
        {
            log::error!(
                "wgc capture size mismatch: got {}x{}, monitor {}x{}",
                full.width,
                full.height,
                monitors::rect_width(&mon.rect),
                monitors::rect_height(&mon.rect)
            );
            return None;
        }
        let iw = monitors::rect_width(&inter) as usize;
        let ih = monitors::rect_height(&inter) as usize;
        let src_x = (inter.left - mon.rect.left) as usize;
        let src_y = (inter.top - mon.rect.top) as usize;
        let dst_x = (inter.left - sel.left) as usize;
        let dst_y = (inter.top - sel.top) as usize;

        for row in 0..ih {
            let src_start = (src_y + row) * full.width as usize * 4 + src_x * 4;
            let dst_start = (dst_y + row) * w as usize * 4 + dst_x * 4;
            out[dst_start..dst_start + iw * 4]
                .copy_from_slice(&full.bgra[src_start..src_start + iw * 4]);
        }
    }
    Some(ImageBuf {
        width: w,
        height: h,
        bgra: out,
    })
}

/// Capture one full monitor into a BGRA buffer at its physical resolution.
pub fn capture_monitor(mon: &MonitorInfo) -> Option<ImageBuf> {
    let w = monitors::rect_width(&mon.rect);
    let h = monitors::rect_height(&mon.rect);
    if w <= 0 || h <= 0 {
        return None;
    }

    unsafe {
        let screen = CreateDCW(
            PCWSTR::null(),
            PCWSTR(mon.device.as_ptr()),
            PCWSTR::null(),
            None,
        );
        if screen.is_invalid() {
            return None;
        }
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp.into());
        let ok = BitBlt(mem, 0, 0, w, h, Some(screen), 0, 0, SRCCOPY).is_ok();

        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [Default::default()],
        };
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bi,
            DIB_RGB_COLORS,
        );

        let _ = SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = DeleteDC(screen);

        if !ok || lines == 0 {
            return None;
        }
        Some(ImageBuf {
            width: w as u32,
            height: h as u32,
            bgra: buf,
        })
    }
}

/// Capture one monitor via Windows Graphics Capture (one-shot), replacing the
/// synchronous full-desktop GDI readback that could wedge the session's shared
/// present path.
///
/// WGC runs on its own thread (`start_free_threaded`); the whole call is bounded
/// by a frame timeout plus a bounded teardown join, so a stuck GPU/driver
/// degrades to a failed capture instead of a system-wide freeze.
pub fn capture_monitor_wgc(mon: &MonitorInfo) -> Option<ImageBuf> {
    let (tx, rx) = mpsc::channel::<Result<ImageBuf, String>>();
    let item = Monitor::from_raw_hmonitor(mon.hmonitor.0);
    let (cursor, border, interval) = compatible_capture_settings(
        CursorCaptureSettings::WithoutCursor,
        DrawBorderSettings::WithoutBorder,
        MinimumUpdateIntervalSettings::Default,
    );
    let settings = Settings::new(
        item,
        cursor,
        border,
        SecondaryWindowSettings::Default,
        interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        ShotFlags { tx },
    );
    let control = match ShotHandler::start_free_threaded(settings) {
        Ok(control) => control,
        Err(e) => {
            log::error!("wgc capture init failed: {e}");
            return None;
        }
    };

    // First frame is delivered right after session start even on a static
    // screen (the frame pool's initial update); anything past this timeout
    // means the capture never produced a frame.
    let shot = match rx.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok(img)) => Some(img),
        Ok(Err(e)) => {
            log::error!("wgc capture failed: {e}");
            None
        }
        Err(_) => {
            log::error!("wgc capture timed out waiting for a frame");
            None
        }
    };

    // Join the WGC thread without risking our own freeze: if the thread is
    // wedged in the driver, leaking it (for the sign-out to clear) is safer
    // than blocking the capture flow.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = control.stop();
        let _ = done_tx.send(());
    });
    // ponytail: a wedged WGC/D3D thread leaks rather than stalls the flow; a
    // stop that never confirms inside 5s leaves the thread to be reaped at
    // session teardown. Revisit if leaks accumulate.
    let _ = done_rx.recv_timeout(Duration::from_secs(5));

    shot
}

/// Flags for the one-shot handler: the result channel. `Sender` is `Send`, so
/// it travels into the WGC thread's `Context`.
pub struct ShotFlags {
    tx: mpsc::Sender<Result<ImageBuf, String>>,
}

/// WGC handler that grabs the first frame into a CPU buffer, hands it back,
/// then stops the capture session.
struct ShotHandler {
    tx: mpsc::Sender<Result<ImageBuf, String>>,
    sent: bool,
}

impl GraphicsCaptureApiHandler for ShotHandler {
    type Flags = ShotFlags;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { tx: ctx.flags.tx, sent: false })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Guard against any frames racing in before the stop lands — send once.
        if !self.sent {
            self.sent = true;
            let result = (|| -> Result<ImageBuf, String> {
                let buf = frame.buffer().map_err(|e| e.to_string())?;
                let mut scratch = Vec::new();
                // WGC staging textures may be row-padded; pack rows tight.
                let pixels = buf.as_nopadding_buffer(&mut scratch).to_vec();
                Ok(ImageBuf { width: buf.width(), height: buf.height(), bgra: pixels })
            })();
            let _ = self.tx.send(result);
        }
        capture_control.stop();
        Ok(())
    }
}

#[allow(dead_code)]
fn _hgd(obj: HGDIOBJ) {
    let _ = obj;
}
