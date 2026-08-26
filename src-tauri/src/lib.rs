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
mod notifier;
mod overlay;
mod settings;
mod thumbnail;
mod tray;

use std::sync::atomic::AtomicBool;
use tauri::RunEvent;

/// Global pause state for the capture hotkey (toggled from the tray).
pub static PAUSED: AtomicBool = AtomicBool::new(false);

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
            tray::init(app.handle())?;
            editor::init(app.handle());
            let _ = thumbnail::hide_all(app.handle());
            let _ = editor::hide(app.handle());
            // Watch for a frozen thumbnail renderer (e.g. after display sleep)
            // and revive it via reload + re-presentation.
            thumbnail::spawn_renderer_watchdog(app.handle().clone());
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    // Close-to-tray: hide instead of quitting.
                    api.prevent_close();
                    let _ = window.hide();
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
        ])
        .build(tauri::generate_context!())
        .expect("error while building SnapDrop application")
        .run(|app, event| {
            if matches!(event, RunEvent::Resumed) {
                thumbnail::recover_after_resume(app);
            }
        });
}
