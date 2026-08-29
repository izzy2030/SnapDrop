use std::ffi::c_void;
use std::fs;
use chrono::Local;
use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, ShowWindow, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE,
};

/// Rebuild the app's windows-0.62 HWND from tauri's HWND (tauri links a
/// different `windows` crate version; both wrap the same raw pointer).
fn to_hwnd(raw: *mut c_void) -> HWND {
    HWND(raw)
}

use crate::{
    capture, clipboard, editor, filename, history, monitors, notifier, ocr, overlay, settings,
    thumbnail, tray,
};

/// Show the main window again in its previous state WITHOUT activating it, so
/// SnapDrop never steals focus from the app the user was working in — it just
/// stays on the taskbar. `pub(crate)` so the video-record cancel command can
/// restore the window after an armed (but cancelled) recording.
pub(crate) fn restore_main_window(app: &AppHandle, was_visible: bool, was_minimized: bool) {
    if !was_visible {
        return;
    }
    let Some(main_win) = app.get_webview_window("main") else {
        return;
    };
    match main_win.hwnd() {
        Ok(tauri_hwnd) => {
            let hwnd = to_hwnd(tauri_hwnd.0);
            let _ = unsafe {
                ShowWindow(
                    hwnd,
                    if was_minimized {
                        SW_SHOWMINNOACTIVE
                    } else {
                        SW_SHOWNOACTIVATE
                    },
                )
            };
        }
        Err(_) => {
            let _ = main_win.show();
        }
    }
}

/// Hide the main window and floating thumbnails so they never appear in a
/// capture. Returns the main window's pre-capture visibility state for the
/// no-activate restore.
fn hide_for_capture(app: &AppHandle) -> (bool, bool) {
    let main_win = app.get_webview_window("main");
    let main_was_visible = main_win
        .as_ref()
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    let main_was_minimized = main_win
        .as_ref()
        .and_then(|w| w.hwnd().ok())
        .map(|h| unsafe { IsIconic(to_hwnd(h.0)).as_bool() })
        .unwrap_or(false);
    if let Some(main_win) = &main_win {
        let _ = main_win.hide();
    }
    let _ = thumbnail::hide_all(app);
    (main_was_visible, main_was_minimized)
}

pub fn run(app: &AppHandle) {
    // The capture flow may run inside a message dispatch on the main thread;
    // contain any panic so it degrades to an error toast instead of killing
    // the process, and restore the windows the flow hides so the app is never
    // left in a broken state.
    crate::debuglog::log("capture flow: start");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_inner(app)));
    if let Err(payload) = result {
        crate::debuglog::log(&format!("capture flow: PANIC {}", crate::panic_message(&payload)));
        log::error!("capture flow panicked: {}", crate::panic_message(&payload));
        if let Some(main_win) = app.get_webview_window("main") {
            let _ = main_win.show();
        }
        // Deliberately do NOT show the thumbnail here: it still renders whatever
        // was on screen before this capture (the flow hides it up front, and if
        // the panic happened before `show_capture` the renderer has the previous
        // capture's stack). Re-showing it would display a stale "previous shot"
        // ghost. The renderer watchdog revives it if a stuck WebView is involved.
        notifier::toast(app, "error", "Capture failed unexpectedly");
    }
}

fn run_inner(app: &AppHandle) {
    let was_visible = thumbnail::is_visible(app);
    let (main_was_visible, main_was_minimized) = hide_for_capture(app);

    // Brief sleep to ensure DWM compositor updates the screen before capture overlay starts
    std::thread::sleep(std::time::Duration::from_millis(50));

    let settings = settings::get(app);

    // The overlay handles delayed capture itself: holding Shift while
    // selecting arms the countdown (duration from the setting).
    let sel = match overlay::run(settings.show_editor_after_capture, settings.capture_delay_secs) {
        Some(s) => s,
        None => {
            crate::debuglog::log("capture flow: overlay cancelled");
            // Cancelled — restore whatever was hidden.
            if was_visible {
                let _ = thumbnail::set_visible(app, true);
            }
            restore_main_window(app, main_was_visible, main_was_minimized);
            return;
        }
    };
    finish_capture(app, sel, main_was_visible, main_was_minimized);
}

/// "Last area" capture (Ctrl+Alt+4): same pipeline, but the overlay opens
/// pre-positioned on the previously captured region — click to re-capture it
/// instantly, drag inside to move, drag an edge to resize, drag elsewhere for
/// a fresh selection.
pub fn run_last_area(app: &AppHandle) {
    crate::debuglog::log("last-area capture flow: start");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_last_area_inner(app)
    }));
    if let Err(payload) = result {
        crate::debuglog::log(&format!(
            "last-area capture flow: PANIC {}",
            crate::panic_message(&payload)
        ));
        log::error!(
            "last-area capture flow panicked: {}",
            crate::panic_message(&payload)
        );
        if let Some(main_win) = app.get_webview_window("main") {
            let _ = main_win.show();
        }
        notifier::toast(app, "error", "Capture failed unexpectedly");
    }
}

fn run_last_area_inner(app: &AppHandle) {
    let was_visible = thumbnail::is_visible(app);
    let (main_was_visible, main_was_minimized) = hide_for_capture(app);

    std::thread::sleep(std::time::Duration::from_millis(50));

    let settings = settings::get(app);
    let initial = settings.last_area.map(|la| RECT {
        left: la.x,
        top: la.y,
        right: la.x + la.width,
        bottom: la.y + la.height,
    });

    // No remembered area yet (first run) — fall back to a normal selection;
    // whatever the user picks becomes the new last area.
    let sel = match initial {
        Some(rect) => overlay::run_last_area(
            settings.show_editor_after_capture,
            settings.capture_delay_secs,
            rect,
        ),
        None => overlay::run(settings.show_editor_after_capture, settings.capture_delay_secs),
    };
    let Some(sel) = sel else {
        crate::debuglog::log("last-area capture flow: overlay cancelled");
        if was_visible {
            let _ = thumbnail::set_visible(app, true);
        }
        restore_main_window(app, main_was_visible, main_was_minimized);
        return;
    };
    finish_capture(app, sel, main_was_visible, main_was_minimized);
}

/// Shared tail of every image-capture flow: capture the region, save it,
/// clipboard/history, editor-or-thumbnail, and restore the main window.
/// Also records the selection as the "last area" for the re-capture hotkey.
fn finish_capture(
    app: &AppHandle,
    sel: overlay::Selection,
    main_was_visible: bool,
    main_was_minimized: bool,
) {
    let settings = settings::get(app);
    if sel.shift_held && settings.capture_delay_secs > 0 {
        crate::debuglog::log(&format!(
            "capture flow: delayed capture active ({}s, shift-held)",
            settings.capture_delay_secs
        ));
    }
    crate::debuglog::log(&format!(
        "capture flow: selection rect={:?} ctrl_held={} show_editor_setting={} -> editor={}",
        sel.rect,
        sel.ctrl_held,
        settings.show_editor_after_capture,
        settings.show_editor_after_capture != sel.ctrl_held
    ));

    // Fresh monitor enumeration handles sleep/wake and monitor changes.
    let monitors = monitors::enumerate();
    let img = capture::capture_region(&sel.rect, &monitors);
    let Some(img) = img else {
        notifier::toast(app, "error", "Couldn't capture screen region");
        return;
    };

    // Remember this selection as the "last area" so Ctrl+Alt+4 can re-open
    // it. Same virtual-screen coordinates as the overlay uses.
    {
        let mut s = settings::get(app);
        s.last_area = Some(settings::LastArea {
            x: sel.rect.left,
            y: sel.rect.top,
            width: monitors::rect_width(&sel.rect),
            height: monitors::rect_height(&sel.rect),
        });
        let _ = settings::save(app, &s);
    }

    let dir = settings::resolved_dir(&settings);
    // Notify when the configured folder was unusable and we fell back to the default.
    if filename::expand_dir(&settings.screenshot_dir) != dir {
        notifier::toast(
            app,
            "error",
            "Screenshot folder is not writable — using Pictures\\SnapDrop.",
        );
    }

    // Save the PNG.
    let path = filename::next_filename(&dir);
    let mut saved_path: Option<String> = None;
    let mut unsaved = false;
    match filename::encode_png(&img.bgra, img.width, img.height) {
        Ok(png) => match fs::write(&path, &png) {
            Ok(()) => {
                saved_path = Some(path.to_string_lossy().to_string());
                log::info!("saved {}", path.display());
            }
            Err(e) => {
                log::error!("save failed: {e}");
                notifier::toast(app, "error", "Couldn't save screenshot");
                unsaved = true;
            }
        },
        Err(e) => {
            log::error!("png encode failed: {e}");
            notifier::toast(app, "error", "Couldn't save screenshot");
            unsaved = true;
        }
    }

    // Clipboard and history are best-effort: a failure (or even a panic in the
    // COM/file code) must never abort the flow before the thumbnail is shown,
    // otherwise the user gets no thumbnail for a capture that was saved fine.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if settings.copy_to_clipboard {
            let path_for_clip = saved_path.as_deref().map(std::path::Path::new);
            if let Err(e) = clipboard::set_image_and_file(
                &img.bgra,
                img.width,
                img.height,
                path_for_clip.unwrap_or(&std::path::PathBuf::from("SnapDrop.png")),
            ) {
                log::warn!("clipboard failed: {e}");
            }
        }
    }));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // History (only for saved files).
        if let Some(p) = &saved_path {
            history::add(app, p.clone(), Local::now().to_rfc3339());
        }
    }));

    let center = (
        (sel.rect.left + monitors::rect_width(&sel.rect) / 2),
        (sel.rect.top + monitors::rect_height(&sel.rect) / 2),
    );

    // Ctrl flips the editor decision from the setting (XOR): with the editor
    // enabled, holding Ctrl while dragging skips it and goes straight to the
    // thumbnail; with the editor disabled, holding Ctrl opens it for that
    // capture. The screenshot is already saved by this point either way.
    if settings.show_editor_after_capture != sel.ctrl_held {
        crate::debuglog::log(&format!(
            "capture flow: saved={:?} -> EDITOR path (unsaved={})",
            saved_path, unsaved
        ));
        // Pause with the annotation editor; Enter confirms, Esc cancels. Either
        // way the editor's event handler closes it and shows the thumbnail.
        let full_b64 = match filename::encode_png(&img.bgra, img.width, img.height) {
            Ok(png) => {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(&png)
            }
            Err(e) => {
                log::error!("full png encode failed: {e}");
                String::new()
            }
        };
        editor::show(app, saved_path, full_b64, img.width, img.height, center);
    } else {
        crate::debuglog::log(&format!(
            "capture flow: saved={:?} -> THUMBNAIL path (unsaved={})",
            saved_path, unsaved
        ));
        // Preview for the thumbnail.
        let preview_b64 = match filename::preview_png(&img.bgra, img.width, img.height, 512) {
            Some(png) => {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(&png)
            }
            None => String::new(),
        };
        thumbnail::show_capture(app, saved_path, preview_b64, img.width, img.height, unsaved, center);
    }
    tray::refresh(app);

    // The flow hid the main window so it never appears in the screenshot;
    // bring it back (without stealing focus) so the app only goes to the tray
    // when the user closes it.
    restore_main_window(app, main_was_visible, main_was_minimized);
}

/// Text capture (OCR hotkey): select a region, recognize the text offline,
/// put it on the clipboard. Nothing is saved to disk and no thumbnail is
/// shown — the whole point is the fastest possible screenshot → text.
pub fn run_text(app: &AppHandle) {
    crate::debuglog::log("text capture flow: start");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_text_inner(app)));
    if let Err(payload) = result {
        crate::debuglog::log(&format!(
            "text capture flow: PANIC {}",
            crate::panic_message(&payload)
        ));
        log::error!(
            "text capture flow panicked: {}",
            crate::panic_message(&payload)
        );
        if let Some(main_win) = app.get_webview_window("main") {
            let _ = main_win.show();
        }
        notifier::toast(app, "error", "Text capture failed unexpectedly");
    }
}

fn run_text_inner(app: &AppHandle) {
    let (main_was_visible, main_was_minimized) = hide_for_capture(app);

    // Brief sleep to ensure DWM compositor updates the screen before capture overlay starts
    std::thread::sleep(std::time::Duration::from_millis(50));

    let Some(sel) = overlay::run(false, 0) else {
        crate::debuglog::log("text capture flow: overlay cancelled");
        restore_main_window(app, main_was_visible, main_was_minimized);
        return;
    };
    crate::debuglog::log(&format!("text capture flow: selection rect={:?}", sel.rect));

    let monitors = monitors::enumerate();
    let Some(img) = capture::capture_region(&sel.rect, &monitors) else {
        notifier::toast(app, "error", "Couldn't capture screen region");
        restore_main_window(app, main_was_visible, main_was_minimized);
        return;
    };

    // Restore the main window right away (without stealing focus) — OCR runs
    // on a worker thread and the user shouldn't wait on it.
    restore_main_window(app, main_was_visible, main_was_minimized);

    let app2 = app.clone();
    std::thread::spawn(move || {
        match ocr::recognize(&img.bgra, img.width, img.height) {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    notifier::toast(&app2, "info", "No text found in that region");
                    return;
                }
                match clipboard::set_text(&text) {
                    Ok(()) => {
                        let chars = trimmed.chars().count();
                        let preview: String = trimmed.chars().take(80).collect();
                        let ellipsis = if chars > 80 { "…" } else { "" };
                        notifier::toast(
                            &app2,
                            "success",
                            &format!("Text copied ({chars} chars): {preview}{ellipsis}"),
                        );
                    }
                    Err(e) => notifier::toast(&app2, "error", &format!("Clipboard failed: {e}")),
                }
            }
            Err(e) => notifier::toast(&app2, "error", &e),
        }
    });
}

/// Video-recording flow (Ctrl+Alt+V): select a region on screen and start
/// recording it to an MP4 immediately. The app hides itself (so it never
/// appears in the footage) and the tray gains a "Stop Recording" item.
pub fn run_video(app: &AppHandle) {
    crate::debuglog::log("video capture flow: start");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_video_inner(app)));
    if let Err(payload) = result {
        crate::debuglog::log(&format!(
            "video capture flow: PANIC {}",
            crate::panic_message(&payload)
        ));
        log::error!(
            "video capture flow panicked: {}",
            crate::panic_message(&payload)
        );
        if let Some(main_win) = app.get_webview_window("main") {
            let _ = main_win.show();
        }
        notifier::toast(app, "error", "Video capture failed unexpectedly");
    }
}

fn run_video_inner(app: &AppHandle) {
    // Don't arm a second region while a recording is already running.
    if crate::video_recording::is_recording_active() {
        notifier::toast(app, "info", "A recording is already active — stop it first.");
        return;
    }
    let (main_was_visible, main_was_minimized) = hide_for_capture(app);

    // Brief sleep to ensure DWM compositor updates the screen before the overlay starts.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // No editor, no delayed capture for video — the region is the recording.
    let Some(sel) = overlay::run(false, 0) else {
        crate::debuglog::log("video capture flow: overlay cancelled");
        restore_main_window(app, main_was_visible, main_was_minimized);
        return;
    };
    crate::debuglog::log(&format!("video capture flow: selection rect={:?}", sel.rect));

    // Remember this selection as the "last area" so Ctrl+Alt+4 (image) and the
    // Settings Start button reuse the same region.
    {
        let mut s = settings::get(app);
        s.last_area = Some(settings::LastArea {
            x: sel.rect.left,
            y: sel.rect.top,
            width: monitors::rect_width(&sel.rect),
            height: monitors::rect_height(&sel.rect),
        });
        let _ = settings::save(app, &s);
    }

    let settings = settings::get(app);
    let dir = settings::resolved_dir(&settings);
    if filename::expand_dir(&settings.screenshot_dir) != dir {
        notifier::toast(
            app,
            "error",
            "Screenshot folder is not writable — using Pictures\\SnapDrop.",
        );
    }
    let path = dir.join(format!(
        "SnapDrop_video_{}.mp4",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    let rect = RECT {
        left: sel.rect.left,
        top: sel.rect.top,
        right: sel.rect.right,
        bottom: sel.rect.bottom,
    };

    // Arm the recording: store the region + path and show the floating
    // toolbar (Rec / cancel). Recording starts when the user clicks Rec; the
    // app stays hidden meanwhile so it can't appear in the footage. If they
    // cancel, `video_record_arm_cancel` restores the main window.
    {
        let rec_state = app.state::<std::sync::Mutex<crate::video_recording::VideoRecorder>>();
        rec_state
            .lock()
            .unwrap()
            .arm(rect, path.to_string_lossy().to_string(), main_was_visible, main_was_minimized);
    }
    crate::toolbar::arm(app, rect);
    // Show the dashed region border right away (it persists through the
    // recording) so the user sees exactly what will be recorded.
    crate::toolbar::show_border(app, rect);
    crate::debuglog::log(&format!(
        "video capture flow: armed region {:?} -> {}",
        rect,
        path.display()
    ));
}
