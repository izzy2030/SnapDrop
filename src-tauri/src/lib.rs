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

/// Set by the main (Settings) window's page on mount
/// (`report_main_renderer_ready`). If it never arrives shortly after
/// startup, the WebView2 renderer likely failed to come up — the "black
/// window after login" symptom — and the startup watchdog reloads the page
/// once so the hang becomes visible (and self-heals) instead of silent.
static MAIN_RENDERER_READY: AtomicBool = AtomicBool::new(false);
static LAST_MAIN_RENDERER_HEARTBEAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Called from the main window's page as soon as it mounts and periodically while alive.
pub fn note_main_renderer_ready() {
    let now = now_millis();
    let was_ready = MAIN_RENDERER_READY.swap(true, std::sync::atomic::Ordering::SeqCst);
    LAST_MAIN_RENDERER_HEARTBEAT.store(now, std::sync::atomic::Ordering::SeqCst);
    if !was_ready {
        crate::debuglog::log("main window renderer ready (page mounted)");
    }
}

/// Check if the main window's renderer has stopped answering heartbeats (e.g. across sleep or idle).
pub fn is_main_renderer_stale() -> bool {
    let last = LAST_MAIN_RENDERER_HEARTBEAT.load(std::sync::atomic::Ordering::SeqCst);
    if last == 0 {
        return true;
    }
    let now = now_millis();
    now.saturating_sub(last) > 10_000
}

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
    pub use crate::video_recording::record_headless;
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(windows)]
    {
        // Prevent WebView2 from suspending occluded windows, killing renderers,
        // or throttling background timers when sent to the system tray for hours.
        std::env::set_var(
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
            "--disable-backgrounding-occluded-windows --disable-background-timer-throttling --disable-renderer-backgrounding",
        );
    }

    let _ = env_logger::try_init();

    let builder = tauri::Builder::default();

    // In production releases, enforce single-instance so multiple background tray
    // processes are prevented. In dev mode (`cargo tauri dev`), skip this plugin
    // so the dev preview instance can launch freely even if the installed app is running.
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        let _ = commands::show_settings_inner(app);
    }));

    builder
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
            // Session context right after (possibly auto-)start. Right after
            // login — especially with Windows Fast Startup or a stale GPU
            // session — the monitor topology/DWM can be degraded; recording
            // this gives a later failure (black window, thumbnail off-screen)
            // its boot context in the log.
            {
                let monitors = crate::monitors::enumerate();
                let virt = crate::monitors::virtual_screen();
                let dwm_on: bool = {
                    #[cfg(windows)]
                    {
                        use windows::Win32::Graphics::Dwm::DwmIsCompositionEnabled;
                        unsafe { DwmIsCompositionEnabled().is_ok() }
                    }
                    #[cfg(not(windows))]
                    {
                        true
                    }
                };
                use tauri_plugin_autostart::ManagerExt;
                let autostart = app.autolaunch().is_enabled().unwrap_or(false);
                crate::debuglog::log(&format!(
                    "startup context: monitors={} virtual={}x{}@({},{}) scales={:?} dwm={} autostart={}",
                    monitors.len(),
                    crate::monitors::rect_width(&virt),
                    crate::monitors::rect_height(&virt),
                    virt.left,
                    virt.top,
                    monitors
                        .iter()
                        .map(|m| format!("{:.2}", m.scale))
                        .collect::<Vec<_>>(),
                    dwm_on,
                    autostart
                ));
            }
            // Main-window renderer readiness: the page reports on mount; if
            // the signal never arrives, the WebView2 renderer is stuck (black
            // window). Reload the page once and log the outcome so a silent
            // black window becomes a logged, self-healed event.
            {
                let hb_app = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(6));
                    if MAIN_RENDERER_READY.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    crate::debuglog::log(
                        "WARNING main window renderer never reported ready (page not mounted; black-window case) — reloading once",
                    );
                    let hb_app2 = hb_app.clone();
                    let _ = hb_app.run_on_main_thread(move || {
                        if let Some(w) = hb_app2.get_webview_window("main") {
                            let _ = w.reload();
                        }
                    });
                    std::thread::sleep(std::time::Duration::from_secs(8));
                    if MAIN_RENDERER_READY.load(std::sync::atomic::Ordering::SeqCst) {
                        crate::debuglog::log("main window renderer recovered after reload");
                    } else {
                        crate::debuglog::log(
                            "WARNING main window renderer STILL not ready after reload",
                        );
                    }
                });
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
            let _ = commands::show_settings_inner(app.handle());
            debuglog::log("main window shown during setup");
            Ok(())
        })
        .on_window_event(|window, event| {
            // The Rust side is the always-alive witness: log focus and
            // lifetime transitions for every window, so a wedge after
            // minimize/suspend has a backend-side trace even when the
            // webview's own voice is frozen. (Tauri's WindowEvent has no
            // minimize/restore variant, so focus + close + destroy is the
            // full signal available here.)
            match event {
                tauri::WindowEvent::Focused(focused) => {
                    debuglog::log(&format!("window {} focus={focused}", window.label()));
                }
                tauri::WindowEvent::Destroyed => {
                    debuglog::log(&format!("window {} destroyed", window.label()));
                }
                _ => {}
            }
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    // Close-to-tray: hide instead of quitting (unless the
                    // user disabled it in Settings, in which case close exits).
                    if settings::get(window.app_handle()).close_to_tray {
                        api.prevent_close();
                        debuglog::log("close-to-tray: X pressed, close prevented");
                        // 1. Immediately hide via Tauri so Tao's internal WindowFlags::VISIBLE
                        // and Wry's controller visibility are kept in sync with the OS.
                        let _ = window.hide();
                        // 2. Also hide the HWND directly via Win32 so the window disappears
                        // instantly even if the main thread is busy with disk I/O.
                        if let Ok(hwnd) = window.hwnd() {
                            unsafe {
                                crate::native_win::hide_window(hwnd.0 as *mut core::ffi::c_void);
                            }
                        }
                        // 3. Background verification: ensure it stayed hidden.
                        let app = window.app_handle().clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(40));
                            if let Some(w) = app.get_webview_window("main") {
                                if !w.is_visible().unwrap_or(true) {
                                    debuglog::log("close-to-tray: main window hidden cleanly");
                                } else {
                                    if let Ok(hwnd) = w.hwnd() {
                                        unsafe {
                                            crate::native_win::hide_window(hwnd.0 as *mut core::ffi::c_void);
                                        }
                                    }
                                    let _ = w.hide();
                                    debuglog::log("close-to-tray: main window hide retried");
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
            commands::capture_video_now,
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
            commands::get_prev_debug_log,
            commands::open_debug_log,
            commands::open_prev_debug_log,
            commands::report_main_renderer_ready,
            commands::ocr_pending_editor_image,
            commands::copy_text,
            commands::video_record_stop,
            commands::video_record_state,
            commands::video_record_begin,
            commands::video_record_arm_cancel,
            commands::video_toggle_mute,
            commands::video_mute_state,
            commands::video_toggle_pause,
            commands::video_pause_state,
        ])
        .manage(Mutex::new(VideoRecorder::new()))
        .build(tauri::generate_context!())
        .expect("error while building SnapDrop application")
        .run(|app, event| match event {
            RunEvent::Resumed => thumbnail::recover_after_resume(app),
            // Mark clean exits so a wedged session (which ends abruptly,
            // mid-line, with no marker) is distinguishable from a normal tray
            // quit in the (now persistent) debug log.
            RunEvent::Exit => {
                crate::debuglog::log("session end (clean exit)");
            }
            _ => {}
        });
}
