use std::fs;
use chrono::Local;
use tauri::{AppHandle, Manager};

use crate::{
    capture, clipboard, editor, filename, history, monitors, notifier, overlay, settings,
    thumbnail, tray,
};

pub fn run(app: &AppHandle) {
    // The capture flow may run inside a message dispatch on the main thread;
    // contain any panic so it degrades to an error toast instead of killing
    // the process, and restore the windows the flow hides so the app is never
    // left in a broken state.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_inner(app)));
    if let Err(payload) = result {
        log::error!("capture flow panicked: {}", crate::panic_message(&payload));
        if let Some(main_win) = app.get_webview_window("main") {
            let _ = main_win.show();
        }
        if let Some(thumb) = app.get_webview_window("thumbnail") {
            let _ = thumb.show();
        }
        notifier::toast(app, "error", "Capture failed unexpectedly");
    }
}

fn run_inner(app: &AppHandle) {
    // Hide main window and floating thumbnails so they never appear in the capture.
    let main_was_visible = app
        .get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    if let Some(main_win) = app.get_webview_window("main") {
        let _ = main_win.hide();
    }

    let was_visible = thumbnail::is_visible(app);
    let _ = thumbnail::hide_all(app);

    // Brief sleep to ensure DWM compositor updates the screen before capture overlay starts
    std::thread::sleep(std::time::Duration::from_millis(50));

    let sel = match overlay::run() {
        Some(s) => s,
        None => {
            // Cancelled — restore whatever was hidden.
            if was_visible {
                let _ = thumbnail::set_visible(app, true);
            }
            if main_was_visible {
                if let Some(main_win) = app.get_webview_window("main") {
                    let _ = main_win.show();
                }
            }
            return;
        }
    };

    // Fresh monitor enumeration handles sleep/wake and monitor changes.
    let monitors = monitors::enumerate();
    let img = capture::capture_region(&sel.rect, &monitors);
    let Some(img) = img else {
        notifier::toast(app, "error", "Couldn't capture screen region");
        return;
    };

    let settings = settings::get(app);
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

    // Clipboard (best-effort; failure must not interrupt the workflow).
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

    // History (only for saved files).
    if let Some(p) = &saved_path {
        history::add(app, p.clone(), Local::now().to_rfc3339());
    }

    let center = (
        (sel.rect.left + monitors::rect_width(&sel.rect) / 2),
        (sel.rect.top + monitors::rect_height(&sel.rect) / 2),
    );

    if settings.show_editor_after_capture {
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
}
