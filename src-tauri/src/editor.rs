//! Annotation editor lifecycle.
//!
//! After a capture the app can pause with a full-screen editor (pen, highlighter,
//! arrow, rectangle, undo) instead of jumping straight to the thumbnail. The
//! editor runs in its own always-on-top webview window. Enter confirms (the
//! annotated image is saved back over the original file), Esc cancels (the
//! original stays). Either way the editor closes and the floating thumbnail
//! appears.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Event, Listener, Manager};

use crate::{clipboard, filename, notifier, settings};

#[derive(Serialize, Clone, Debug)]
pub struct EditorPayload {
    /// Full-resolution PNG of the capture as a data URL (base64).
    pub full: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Deserialize, Clone)]
pub struct ConfirmPayload {
    /// Full-resolution annotated PNG as a data URL (base64).
    pub annotated: String,
}

/// Pending capture state used when the editor confirms/cancels.
struct Pending {
    path: Option<String>,
    /// Full-resolution PNG base64 (no data-URL prefix).
    full_b64: String,
    width: u32,
    height: u32,
    center: (i32, i32),
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

pub fn is_visible(app: &AppHandle) -> bool {
    app.get_webview_window("editor")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

pub fn hide(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("editor") {
        let _ = w.hide();
    }
}

/// Show the editor for a freshly captured screenshot.
/// `full_b64` is the full-resolution PNG (base64, no data-URL prefix).
pub fn show(
    app: &AppHandle,
    path: Option<String>,
    full_b64: String,
    img_w: u32,
    img_h: u32,
    center: (i32, i32),
) {
    crate::debuglog::log(&format!(
        "editor: show path={:?} full_b64_len={}",
        path,
        full_b64.len()
    ));
    *PENDING.lock().unwrap() = Some(Pending {
        path,
        full_b64: full_b64.clone(),
        width: img_w,
        height: img_h,
        center,
    });

    let Some(w) = app.get_webview_window("editor") else {
        crate::debuglog::log("editor: show -> editor window MISSING");
        return;
    };

    // Size to the monitor that holds the capture so the editor covers that
    // screen; a small margin keeps a sliver of the desktop visible for context.
    let mon = crate::monitors::monitor_at_point(center.0, center.1)
        .or_else(|| crate::monitors::monitor_at_point(0, 0));
    if let Some(m) = mon {
        use tauri::{PhysicalPosition, PhysicalSize, Size};
        let (wl, wt) = (m.work.left, m.work.top);
        let (ww, wh) = (m.work.right - m.work.left, m.work.bottom - m.work.top);
        let _ = w.set_size(Size::Physical(PhysicalSize::new(ww as u32, wh as u32)));
        let _ = w.set_position(PhysicalPosition::new(wl, wt));
    }

    let _ = w.show();
    let _ = w.unminimize();
    let _ = w.set_always_on_top(true);
    let _ = w.set_focus();
    let payload = EditorPayload {
        full: format!("data:image/png;base64,{full_b64}"),
        width: img_w,
        height: img_h,
    };
    // Use an editor-specific event name so the thumbnail and settings windows
    // cannot consume this different payload contract while the editor is shown.
    let _ = app.emit("editor-captured", payload);
}

/// IPC command: the editor fetches the pending capture image on mount (in case
/// the editor event was emitted before its listener was ready).
pub fn get_pending_image() -> Option<EditorPayload> {
    let guard = PENDING.lock().unwrap();
    guard.as_ref().map(|p| EditorPayload {
        full: format!("data:image/png;base64,{}", p.full_b64),
        width: p.width,
        height: p.height,
    })
}

/// The pending capture as tightly-packed BGRA (for OCR). Decoded on demand
/// from the stored full-resolution PNG.
pub fn pending_bgra() -> Option<(Vec<u8>, u32, u32)> {
    let guard = PENDING.lock().unwrap();
    let p = guard.as_ref()?;
    let png = base64_decode(&p.full_b64)?;
    let img = image::load_from_memory(&png).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for px in rgba.pixels() {
        bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    Some((bgra, w, h))
}

/// Registers the editor's confirm/cancel event listeners. Called once at startup.
pub fn init(app: &AppHandle) {
    let confirm_app = app.clone();
    let confirm_app2 = app.clone();
    let _ = confirm_app.listen("editor-confirmed", move |event: Event| {
        let payload: ConfirmPayload = match serde_json::from_str::<ConfirmPayload>(
            event.payload(),
        ) {
            Ok(p) => p,
            Err(_) => return,
        };
        let app = confirm_app2.clone();
        tauri::async_runtime::spawn(async move {
            on_confirmed(&app, payload);
        });
    });

    let cancel_app = app.clone();
    let cancel_app2 = app.clone();
    let _ = cancel_app.listen("editor-cancelled", move |_: Event| {
        let app = cancel_app2.clone();
        tauri::async_runtime::spawn(async move {
            on_cancelled(&app);
        });
    });
}

fn take_pending(app: &AppHandle) -> Option<Pending> {
    let mut guard = PENDING.lock().unwrap();
    if !is_visible(app) {
        return None;
    }
    guard.take()
}

fn on_confirmed(app: &AppHandle, payload: ConfirmPayload) {
    crate::debuglog::log(&format!(
        "editor: confirmed received (annotated len={})",
        payload.annotated.len()
    ));
    log::info!("editor: editor-confirmed received (annotated len={})", payload.annotated.len());
    let Some(pending) = take_pending(app) else {
        crate::debuglog::log("editor: on_confirmed SKIPPED (no pending or editor not visible)");
        log::warn!("editor: on_confirmed skipped (no pending or editor not visible)");
        return;
    };
    let settings = settings::get(app);

    // Decode the annotated PNG (data URL: data:image/png;base64,....).
    let b64 = payload
        .annotated
        .split_once(',')
        .map(|(_, b)| b.to_string())
        .unwrap_or(payload.annotated.clone());
    let decoded = match base64_decode(&b64) {
        Some(d) => d,
        None => {
            notifier::toast(app, "error", "Couldn't process the annotated image");
            finish(app, pending.path, pending.width, pending.height, pending.center);
            return;
        }
    };

    // Save the annotated image. Replace the original file; if the original save
    // failed, write to a fresh filename in the configured folder.
    let dir = settings::resolved_dir(&settings);
    let out_path: PathBuf = match &pending.path {
        Some(p) => PathBuf::from(p),
        None => filename::next_filename(&dir),
    };

    let saved_path: Option<String> = match fs::write(&out_path, &decoded) {
        Ok(()) => {
            log::info!("saved annotated {}", out_path.display());
            Some(out_path.to_string_lossy().to_string())
        }
        Err(e) => {
            log::error!("annotated save failed: {e}");
            notifier::toast(app, "error", "Couldn't save annotated screenshot");
            None
        }
    };
    log::info!("editor: saved_path={:?}", saved_path);

    // The annotated image replaces the clipboard copy too.
    if settings.copy_to_clipboard {
        if let Some(p) = &saved_path {
            if let Ok(img) = image::open(p) {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
                for px in rgba.pixels() {
                    bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
                let _ = clipboard::set_image_and_file(&bgra, w, h, std::path::Path::new(p));
            }
        }
    }

    finish(app, saved_path, pending.width, pending.height, pending.center);
}

fn on_cancelled(app: &AppHandle) {
    crate::debuglog::log("editor: cancelled received");
    log::info!("editor: editor-cancelled received");
    let Some(pending) = take_pending(app) else {
        crate::debuglog::log("editor: on_cancelled SKIPPED (no pending or editor not visible)");
        log::warn!("editor: on_cancelled skipped (no pending or editor not visible)");
        return;
    };
    finish(app, pending.path, pending.width, pending.height, pending.center);
}

/// Close the editor and present the floating thumbnail.
fn finish(app: &AppHandle, path: Option<String>, _img_w: u32, _img_h: u32, center: (i32, i32)) {
    crate::debuglog::log(&format!("editor: finish path={:?} -> show_capture_for_at", path));
    log::info!("editor: finish (path={:?})", path);
    hide(app);
    match path {
        // Reload the (possibly annotated) file so the thumbnail preview is fresh,
        // positioned on the monitor where the capture happened.
        Some(p) => crate::thumbnail::show_capture_for_at(app, &p, Some(center)),
        None => crate::thumbnail::show_capture(
            app,
            None,
            String::new(),
            _img_w,
            _img_h,
            true,
            center,
        ),
    }
}

fn base64_decode(b64: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}
