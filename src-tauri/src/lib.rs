mod capture;
mod capture_flow;
mod clipboard;
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

/// Global pause state for the capture hotkey (toggled from the tray).
pub static PAUSED: AtomicBool = AtomicBool::new(false);

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
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            settings::init(app.handle())?;
            history::init(app.handle())?;
            hotkey::init(app.handle());
            tray::init(app.handle())?;
            editor::init(app.handle());
            let _ = thumbnail::hide_all(app.handle());
            let _ = editor::hide(app.handle());
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
