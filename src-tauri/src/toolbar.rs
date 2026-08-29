//! Floating recording toolbar — Rec/Stop, elapsed time, mute, cancel.
//!
//! A tiny always-on-top, frameless, transparent window shown while a
//! recording is *armed* (region selected, waiting for the user to hit Rec)
//! and while recording is active. It's excluded from screen capture via
//! `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` so SnapDrop never
//! films its own toolbar — the user can drag it anywhere, even over the
//! region being recorded, without it appearing in the footage.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};

// Must match the `recorder_toolbar` window in tauri.conf.json.
const TOOLBAR_W: i32 = 360;
const TOOLBAR_H: i32 = 80;
const MARGIN: i32 = 12;
/// Padding (physical px) around the recording region on the border window,
/// giving the dimension pill room to sit just outside the top-left corner
/// (above the dashed line, clear of the corner handle). Must match
/// `PAD_PHYSICAL` in src/recorder_border.tsx.
const BORDER_PAD: i32 = 48;

/// Global "system audio muted" flag, read by the recorder handler (it zeroes
/// the PCM so the audio timeline stays continuous instead of dropping
/// samples, which would break A/V sync).
static MUTED: AtomicBool = AtomicBool::new(false);
/// Global "recording paused" flag. Read every frame by the recorder handler
/// (which freezes feeding the encoder so the video AND audio timelines both
/// stop advancing — keeping A/V synced across a pause). Toggled by the
/// toolbar's Pause/Resume button via a command.
static PAUSED: AtomicBool = AtomicBool::new(false);

pub fn is_muted() -> bool {
    MUTED.load(Ordering::SeqCst)
}

/// Flip the muted flag; returns the new state (for the toolbar UI).
pub fn toggle_mute() -> bool {
    let next = !is_muted();
    MUTED.store(next, Ordering::SeqCst);
    next
}

pub fn is_paused() -> bool {
    PAUSED.load(Ordering::SeqCst)
}

/// Flip the pause flag; returns the new state (for the toolbar UI).
pub fn toggle_pause() -> bool {
    let next = !is_paused();
    PAUSED.store(next, Ordering::SeqCst);
    next
}

#[derive(Clone, Serialize)]
pub struct RecorderState {
    pub recording: bool,
    pub muted: bool,
    pub paused: bool,
}

/// Make the toolbar window invisible to screen-capture APIs (WGC included),
/// so it never shows up in the recording no matter where the user drags it.
fn apply_exclude_from_capture(app: &AppHandle) {
    let Some(win) = app.get_webview_window("recorder_toolbar") else {
        return;
    };
    apply_exclude_from_capture_to(&win);
}

/// Apply `WDA_EXCLUDEFROMCAPTURE` to a given webview window so it never
/// appears in screen captures (WGC included).
fn apply_exclude_from_capture_to(win: &tauri::WebviewWindow) {
    if let Ok(hwnd) = win.hwnd() {
        let h = windows::Win32::Foundation::HWND(hwnd.0);
        unsafe {
            let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
        }
    }
}

/// Position the toolbar just below the recording region (falling back to just
/// above it, then clamped inside the monitor).
fn place(app: &AppHandle, region: RECT) {
    let Some(win) = app.get_webview_window("recorder_toolbar") else {
        return;
    };
    let cx = region.left + (region.right - region.left) / 2;
    let cy = region.top + (region.bottom - region.top) / 2;
    let Some(mon) = crate::monitors::monitor_at_point(cx, cy) else {
        return;
    };
    let m = mon.rect;
    let mut x = cx - TOOLBAR_W / 2;
    let mut y = region.bottom + MARGIN;
    if y + TOOLBAR_H > m.bottom {
        y = region.top - TOOLBAR_H - MARGIN;
    }
    x = x.clamp(m.left + 8, m.right - TOOLBAR_W - 8);
    y = y.clamp(m.top + 8, m.bottom - TOOLBAR_H - 8);
    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
}

/// Show the toolbar in "armed" mode: region selected, waiting for the user
/// to hit Rec to start recording (or X to cancel).
pub fn arm(app: &AppHandle, region: RECT) {
    place(app, region);
    apply_exclude_from_capture(app);
    if let Some(win) = app.get_webview_window("recorder_toolbar") {
        let _ = win.show();
    }
    let _ = app.emit(
        "video_recorder_state",
        RecorderState { recording: false, muted: is_muted(), paused: is_paused() },
    );
}

/// Show the toolbar in recording mode (elapsed timer + mute + stop).
pub fn show_recording(app: &AppHandle) {
    // A fresh recording must never start already-paused (the flag can be left
    // true if the user paused the previous take before stopping).
    PAUSED.store(false, Ordering::SeqCst);
    apply_exclude_from_capture(app);
    if let Some(win) = app.get_webview_window("recorder_toolbar") {
        let _ = win.show();
    }
    let _ = app.emit(
        "video_recorder_state",
        RecorderState { recording: true, muted: is_muted(), paused: is_paused() },
    );
}

/// Hide the toolbar (recording ended or cancelled).
pub fn hide(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("recorder_toolbar") {
        let _ = win.hide();
    }
}

/// Show the dashed border + dimension pill around the recording region. The
/// window is a few pixels larger than the region (padding for the label),
/// excluded from screen capture via `WDA_EXCLUDEFROMCAPTURE` (so it never
/// appears in the footage), and click-through (`set_ignore_cursor_events`)
/// so it doesn't block interaction with the app being recorded.
pub fn show_border(app: &AppHandle, region: RECT) {
    let Some(win) = app.get_webview_window("recorder_border") else {
        return;
    };
    let w = (region.right - region.left).max(16) + BORDER_PAD * 2;
    let h = (region.bottom - region.top).max(16) + BORDER_PAD * 2;
    let _ = win.set_position(tauri::PhysicalPosition::new(
        region.left - BORDER_PAD,
        region.top - BORDER_PAD,
    ));
    let _ = win.set_size(tauri::PhysicalSize::new(w, h));
    apply_exclude_from_capture_to(&win);
    let _ = win.set_ignore_cursor_events(true);
    let _ = win.show();
}

/// Hide the region border (recording ended or cancelled).
pub fn hide_border(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("recorder_border") {
        let _ = win.hide();
    }
}
