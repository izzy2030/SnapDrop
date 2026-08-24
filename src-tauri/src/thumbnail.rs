//! Floating thumbnail window: sizing, positioning, and capture events.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Size};

use crate::{monitors, settings};

#[derive(Serialize, Clone)]
pub struct CapturedPayload {
    /// None when the screenshot could not be saved (drag disabled, "not saved" state).
    pub path: Option<String>,
    /// data:image/png;base64,...
    pub preview: String,
    pub width: u32,
    pub height: u32,
    pub unsaved: bool,
}

pub fn hide_all(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("thumbnail") {
        let _ = w.hide();
    }
    Ok(())
}

pub fn is_visible(app: &AppHandle) -> bool {
    app.get_webview_window("thumbnail")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

pub fn set_visible(app: &AppHandle, visible: bool) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("thumbnail") {
        if visible {
            let _ = w.show();
        } else {
            let _ = w.hide();
        }
    }
    Ok(())
}

/// Show the thumbnail for a freshly captured screenshot.
/// `sel_center` is the selection center in virtual-screen coords (used to pick the monitor).
pub fn show_capture(
    app: &AppHandle,
    path: Option<String>,
    preview_b64: String,
    img_w: u32,
    img_h: u32,
    unsaved: bool,
    sel_center: (i32, i32),
) {
    let settings = settings::get(app);
    if !settings.show_thumbnail {
        return;
    }

    // Center of the selection → monitor for positioning.
    let mon = monitors::monitor_at_point(sel_center.0, sel_center.1)
        .or_else(|| monitors::monitor_at_point(0, 0));
    let Some(mon) = mon else {
        return;
    };

    let base = match settings.thumbnail_size.as_str() {
        "small" => 220,
        "large" => 420,
        _ => 320,
    };
    let aspect = if img_w > 0 && img_h > 0 {
        img_h as f64 / img_w as f64
    } else {
        0.75
    };
    let win_w = base;
    let win_h = ((base as f64 * aspect).round() as u32).clamp(120, 620);
    let margin = 16i32;

    // Position within the monitor's work area so the thumbnail never sits under the taskbar.
    let (wl, wt, wr, wb) = (mon.work.left, mon.work.top, mon.work.right, mon.work.bottom);
    let (wx, wy) = match settings.thumbnail_position.as_str() {
        "top_left" => (wl + margin, wt + margin),
        "top_right" => (wr - win_w as i32 - margin, wt + margin),
        "bottom_left" => (wl + margin, wb - win_h as i32 - margin),
        _ => (wr - win_w as i32 - margin, wb - win_h as i32 - margin),
    };

    if let Some(w) = app.get_webview_window("thumbnail") {
        let _ = w.set_size(Size::Physical(PhysicalSize::new(win_w, win_h)));
        let _ = w.set_position(PhysicalPosition::new(wx, wy));
        let shown = w.show();
        log::info!(
            "thumbnail::show_capture pos=({wx},{wy}) size={win_w}x{win_h} show={:?} path={:?}",
            shown.as_ref().map(|_| "ok"),
            path
        );
        let _ = w.emit(
            "captured",
            CapturedPayload {
                path,
                preview: format!("data:image/png;base64,{preview_b64}"),
                width: img_w,
                height: img_h,
                unsaved,
            },
        );
    } else {
        log::error!("thumbnail::show_capture: thumbnail window not found");
    }
}

/// Show the thumbnail for an existing file (tray "Recent Captures").
pub fn show_capture_for(app: &AppHandle, path: &str) {
    show_capture_for_at(app, path, None);
}

/// Show the thumbnail for an existing file, positioned near the given monitor
/// point (`sel_center` in virtual-screen coords).
pub fn show_capture_for_at(app: &AppHandle, path: &str, sel_center: Option<(i32, i32)>) {
    let img = match image::open(path) {
        Ok(i) => i,
        Err(e) => {
            crate::notifier::toast(app, "error", &format!("Could not open {path}: {e}"));
            return;
        }
    };
    use image::GenericImageView;
    let (w, h) = img.dimensions();
    let max_dim = 512u32;
    let (tw, th) = if w.max(h) <= max_dim {
        (w, h)
    } else {
        let s = max_dim as f64 / w.max(h) as f64;
        (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
    };
    // `thumbnail()` preserves aspect ratio and may return a slightly smaller image
    // than the requested `tw`/`th` (e.g. 511×421 vs 512×421 due to rounding), so
    // use the actual returned dimensions for encoding to avoid a buffer-size panic.
    let thumb = img.thumbnail(tw, th);
    let (actual_tw, actual_th) = thumb.dimensions();
    let thumb = thumb.to_rgba8();
    let mut png = Vec::new();
    use image::ImageEncoder;
    let ok = image::codecs::png::PngEncoder::new(&mut png).write_image(
        thumb.as_raw(),
        actual_tw,
        actual_th,
        image::ExtendedColorType::Rgba8,
    );
    if ok.is_err() {
        crate::notifier::toast(app, "error", "Could not preview capture");
        return;
    }
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    let center = sel_center.unwrap_or(((w / 2) as i32, (h / 2) as i32));
    show_capture(app, Some(path.to_string()), b64, w, h, false, center);
}
