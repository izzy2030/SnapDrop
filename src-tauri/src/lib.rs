mod capture;
mod capture_flow;
mod clipboard;
mod debuglog;
mod commands;
mod dpi;
mod dragdrop;
mod editor;
mod filename;
mod history;
mod hotkey;
mod monitors;
mod native_thumb;
mod native_win;
mod ocr;
mod notifier;
mod overlay;
mod settings;
mod thumbnail;
mod toolbar;
mod tray;

#[cfg(windows)]
mod video_recording;
mod audio;
#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use video_recording::VideoRecorder;

use std::sync::atomic::AtomicBool;
use tauri::{Manager, RunEvent};

/// Global pause state for the capture hotkey (toggled from the tray).
pub static PAUSED: AtomicBool = AtomicBool::new(false);

/// Incremented by a tiny task that runs on the main event loop each tick.
/// A side-thread watchdog compares it against its own copy: if the loop ever
/// stops pumping (wedged main thread), nothing else can log it — the loop is
/// the thing doing the logging — so this is the only way to see it happen.
static MAIN_LOOP_HEARTBEAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Extract a human-readable message from a panic payload.
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

pub fn init_dpi() {
    dpi::set_per_monitor_dpi_awareness();
}

/// Re-exports used by examples/smoke tests (not part of the app API).
pub mod test_helpers {
    pub use crate::capture::{capture_monitor, capture_region, ImageBuf};
    pub use crate::filename::encode_png;
    pub use crate::monitors::{enumerate as enumerate_monitors, MonitorInfo};
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = env_logger::try_init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            use tauri::Manager;
            debuglog::init(app.handle());
            debuglog::log("app setup");
            settings::init(app.handle())?;
            history::init(app.handle())?;
            hotkey::init(app.handle());
            hotkey::init_video_hotkey(app.handle());
            tray::init(app.handle())?;
            editor::init(app.handle());
            let _ = thumbnail::hide_all(app.handle());
            let _ = editor::hide(app.handle());
            // The native thumbnail replaced the WebView2 renderer. Left alive,
            // the config-defined "thumbnail" window would still mount its page
            // and poll `get_latest_capture` every 1.5s forever — a 250KB+ IPC
            // payload plus debug-log writes on the main thread, every tick, for
            // nothing. All webview code paths guard on the window being gone.
            if thumbnail::NATIVE_THUMBNAIL {
                if let Some(w) = app.get_webview_window("thumbnail") {
                    let _ = w.close();
                }
            }
            // Watch for a frozen thumbnail renderer (e.g. after display sleep)
            // and revive it via reload + re-presentation.
            thumbnail::spawn_renderer_watchdog(app.handle().clone());
            // Heartbeat watchdog: if the main event loop ever stops pumping
            // (wedged main thread), close-to-tray, hotkeys, everything goes
            // dead — and the loop itself is what logs, so it would be silent.
            // Post a ping every 1.5s and confirm it ran; log if it doesn't.
            {
                let hb_app = app.handle().clone();
                std::thread::spawn(move || {
                    let mut missed = 0u32;
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        let before = MAIN_LOOP_HEARTBEAT.load(std::sync::atomic::Ordering::SeqCst);
                        if hb_app
                            .run_on_main_thread(|| {
                                MAIN_LOOP_HEARTBEAT
                                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            })
                            .is_err()
                        {
                            return; // event loop gone — app shutting down
                        }
                        // Give the loop a beat to run the ping; a healthy loop
                        // pumps it in ms.
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        let now = MAIN_LOOP_HEARTBEAT.load(std::sync::atomic::Ordering::SeqCst);
                        if now == before {
                            missed += 1;
                            if missed == 2 {
                                debuglog::log(
                                    "WARNING main event loop unresponsive (heartbeat missed; close/hotkeys may appear dead)",
                                );
                            } else if missed >= 3 {
                                debuglog::log(&format!(
                                    "WARNING main event loop still unresponsive ({missed} heartbeats missed)"
                                ));
                            }
                        } else if missed >= 2 {
                            debuglog::log(
                                "main event loop responsive again (heartbeat recovered)",
                            );
                            missed = 0;
                        } else {
                            missed = 0;
                        }
                    }
                });
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
                debuglog::log("main window shown during setup");
            } else {
                debuglog::log("ERROR: main window was not found during setup");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    // Close-to-tray: hide instead of quitting (unless the
                    // user disabled it in Settings, in which case close exits).
                    if settings::get(window.app_handle()).close_to_tray {
                        api.prevent_close();
                        debuglog::log("close-to-tray: X pressed, close prevented");
                        // Hiding via tauri's Window::hide() goes through the
                        // main-thread event loop. If the main thread is busy
                        // (a slow debug-log disk write, a wedged hotkey/capture
                        // task), that hide never runs and the X "does nothing".
                        // So hide the HWND directly with ShowWindow(SW_HIDE) -
                        // callable from any thread, no event loop required -
                        // then verify, with fallbacks through Tauri if needed.
                        let app = window.app_handle().clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(30));
                            let mut hidden = false;
                            // Pass 1: direct Win32 hide (independent of the
                            // main-thread loop, which may be wedged).
                            if let Some(w) = app.get_webview_window("main") {
                                if let Ok(hwnd) = w.hwnd() {
                                    unsafe {
                                        crate::native_win::hide_window(hwnd.0 as *mut core::ffi::c_void);
                                    }
                                }
                                if !w.is_visible().unwrap_or(true) {
                                    debuglog::log("close-to-tray: main window hidden (direct ShowWindow)");
                                    hidden = true;
                                }
                            }
                            // Pass 2: if still visible, retry via the normal
                            // Tauri hide (works when the loop is merely busy,
                            // not wedged).
                            if !hidden {
                                for attempt in 1..=3 {
                                    let attempt_no = attempt;
                                    let app_for_task = app.clone();
                                    let _ = app.clone().run_on_main_thread(move || {
                                        let Some(w) = app_for_task.get_webview_window("main") else {
                                            return;
                                        };
                                        if !w.is_visible().unwrap_or(false) {
                                            return; // already hidden
                                        }
                                        match w.hide() {
                                            Ok(()) => {
                                                if w.is_visible().unwrap_or(true) {
                                                    debuglog::log(&format!(
                                                        "close-to-tray: hide attempt {attempt_no} reported ok but window STILL visible"
                                                    ));
                                                } else {
                                                    debuglog::log(
                                                        "close-to-tray: main window hidden",
                                                    );
                                                }
                                            }
                                            Err(e) => debuglog::log(&format!(
                                                "close-to-tray: hide failed on attempt {attempt_no}: {e}"
                                            )),
                                        }
                                    });
                                    // Retry gap off the main thread; if the first
                                    // hide stuck, a second hide is a harmless no-op.
                                    std::thread::sleep(std::time::Duration::from_millis(80));
                                }
                            }
                        });
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::update_settings,
            commands::get_history,
            commands::delete_capture,
            commands::clear_history,
            commands::open_capture,
            commands::reveal_capture,
            commands::open_folder,
            commands::start_drag,
            commands::copy_capture,
            commands::capture_now,
            commands::hide_thumbnail,
            commands::pause_hotkey,
            commands::get_app_version,
            commands::get_pending_editor_image,
            commands::show_settings,
            commands::pick_folder,
            commands::get_capture_preview,
            commands::get_latest_capture,
            commands::debug_log,
            commands::report_renderer_input,
            commands::report_renderer_pointerdown,
            commands::get_debug_log,
            commands::open_debug_log,
            commands::ocr_pending_editor_image,
            commands::copy_text,
            commands::video_record_stop,
            commands::video_record_state,
            commands::video_record_begin,
            commands::video_record_arm_cancel,
            commands::video_toggle_mute,
            commands::video_mute_state,
        ])
        .manage(Mutex::new(VideoRecorder::new()))
        .build(tauri::generate_context!())
        .expect("error while building SnapDrop application")
        .run(|app, event| {
            if matches!(event, RunEvent::Resumed) {
                thumbnail::recover_after_resume(app);
            }
        });
}
