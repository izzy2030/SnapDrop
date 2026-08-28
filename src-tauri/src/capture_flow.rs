use std::ffi::c_void;
use std::fs;
use chrono::Local;
use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, ShowWindow, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE,
};

/// Rebuild the app's windows-0.62 HWND from tauri's HWND (tauri links a
/// different `windows` crate version; both wrap the same raw pointer).
fn to_hwnd(raw: *mut c_void) -> HWND {
    HWND(raw)
}

use crate::{
    capture, clipboard, editor, filename, history, monitors, notifier, overlay, settings,
    thumbnail, tray,
};

/// Show the main window again in its previous state WITHOUT activating it, so
/// SnapDrop never steals focus from the app the user was working in — it just
/// stays on the taskbar.
fn restore_main_window(app: &AppHandle, was_visible: bool, was_minimized: bool) {
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
    // Hide main window and floating thumbnails so they never appear in the capture.
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

    let was_visible = thumbnail::is_visible(app);
    let _ = thumbnail::hide_all(app);

    // Brief sleep to ensure DWM compositor updates the screen before capture overlay starts
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Snapshot settings up front: the overlay needs the editor toggle to label
    // its Ctrl hint, and nothing here changes mid-capture.
    let settings = settings::get(app);

    let sel = match overlay::run(settings.show_editor_after_capture) {
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
