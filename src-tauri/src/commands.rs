//! Tauri IPC commands.

use std::fs;
use std::path::Path;

use tauri::{AppHandle, Manager};

use crate::{clipboard, dragdrop, filename, history, hotkey, settings, thumbnail};

#[tauri::command]
pub fn get_settings(app: AppHandle) -> settings::Settings {
    settings::get(&app)
}

#[tauri::command]
pub fn update_settings(app: AppHandle, settings_new: settings::Settings) -> Result<(), String> {
    let old = settings::get(&app);

    // Validate the screenshot directory before accepting changes.
    let dir = filename::expand_dir(&settings_new.screenshot_dir);
    if let Err(e) = filename::ensure_dir_writable(&dir) {
        return Err(e);
    }

    // Re-register hotkey if it changed (conflict → error, keep old).
    if settings_new.hotkey != old.hotkey {
        if let Err(e) = hotkey::apply_settings(&app, &settings_new.hotkey) {
            return Err(e);
        }
    }

    // Autostart toggle.
    if settings_new.start_with_windows != old.start_with_windows {
        use tauri_plugin_autostart::ManagerExt;
        if settings_new.start_with_windows {
            let _ = app.autolaunch().enable();
        } else {
            let _ = app.autolaunch().disable();
        }
    }

    settings::save(&app, &settings_new)?;
    crate::tray::refresh(&app);
    Ok(())
}

#[tauri::command]
pub fn get_history(app: AppHandle) -> Vec<history::HistoryEntry> {
    history::entries(&app)
}

#[tauri::command]
pub fn delete_capture(app: AppHandle, path: String) -> Result<(), String> {
    let _ = fs::remove_file(&path);
    history::remove(&app, &path);
    crate::tray::refresh(&app);
    Ok(())
}

#[tauri::command]
pub fn clear_history(app: AppHandle) -> Result<(), String> {
    history::clear(&app);
    crate::tray::refresh(&app);
    Ok(())
}

#[tauri::command]
pub fn open_capture(_app: AppHandle, path: String) -> Result<(), String> {
    open_with_default_app(&path)
}

#[tauri::command]
pub fn reveal_capture(app: AppHandle, path: String) -> Result<(), String> {
    let result = reveal_in_explorer(&path);
    // Same dismissal logic as a successful drop: once the capture has been
    // handed off (revealed in Explorer), hide the floating thumbnail if the
    // user has auto-dismiss enabled.
    if result.is_ok() {
        let s = settings::get(&app);
        if s.hide_after_drop {
            let _ = thumbnail::hide_all(&app);
        }
    }
    result
}

#[tauri::command]
pub fn open_folder(app: AppHandle) -> Result<(), String> {
    open_folder_inner(&app)
}

pub fn open_folder_inner(app: &AppHandle) -> Result<(), String> {
    let dir = settings::resolved_dir(&settings::get(app));
    let dir_str = dir.to_string_lossy().to_string();
    std::process::Command::new("explorer.exe")
        .arg(&dir_str)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn start_drag(app: AppHandle, path: String) -> Result<dragdrop::DragOutcome, String> {
    let outcome = dragdrop::start_drag(&app, &path)?;
    if outcome.moved {
        let s = settings::get(&app);
        if !s.keep_after_drag {
            let _ = fs::remove_file(&path);
            history::remove(&app, &path);
            crate::tray::refresh(&app);
        }
    }
    Ok(outcome)
}

#[tauri::command]
pub fn copy_capture(_app: AppHandle, path: String) -> Result<(), String> {
    let p = Path::new(&path);
    // Re-copy the image + file to the clipboard.
    if let Ok(img) = image::open(p) {
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for px in rgba.pixels() {
            bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        clipboard::set_image_and_file(&bgra, w, h, p)?;
    } else {
        clipboard::set_file(p)?;
    }
    Ok(())
}

#[tauri::command]
pub fn capture_now(app: AppHandle) -> Result<(), String> {
    hotkey::trigger_capture(&app);
    Ok(())
}

#[tauri::command]
pub fn hide_thumbnail(app: AppHandle) -> Result<(), String> {
    thumbnail::hide_all(&app)
}

#[tauri::command]
pub fn pause_hotkey(app: AppHandle, paused: bool) -> Result<(), String> {
    crate::PAUSED.store(paused, std::sync::atomic::Ordering::SeqCst);
    hotkey::pause(&app, paused);
    crate::tray::refresh(&app);
    Ok(())
}

#[tauri::command]
pub fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
pub fn get_pending_editor_image() -> Option<crate::editor::EditorPayload> {
    crate::editor::get_pending_image()
}

#[tauri::command]
pub fn show_settings(app: AppHandle) -> Result<(), String> {
    show_settings_inner(&app)
}

pub fn show_settings_inner(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    Ok(())
}

#[tauri::command]
pub fn pick_folder(current: String) -> Result<Option<String>, String> {
    let dialog = rfd::FileDialog::new().set_title("Choose Screenshot Folder");
    let dialog = if current.is_empty() {
        dialog
    } else {
        dialog.set_directory(&current)
    };
    let picked = dialog.pick_folder();
    Ok(picked.map(|p| p.to_string_lossy().to_string()))
}

pub fn open_with_default_app(path: &str) -> Result<(), String> {
    unsafe {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::w!("open"),
            windows::core::PCWSTR(wide.as_ptr()),
            None,
            None,
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
    Ok(())
}

pub fn reveal_in_explorer(path: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, GetWindowTextW, IsIconic, SetForegroundWindow, ShowWindow,
        SwitchToThisWindow, SW_RESTORE,
    };

    // Reuse an existing Explorer window that is already showing the screenshots
    // folder instead of opening a new one every time. Explorer titles its
    // windows "<FolderName> - File Explorer" (Win11) or "<FolderName>" (Win10),
    // so match on the folder-name prefix.
    let p = std::path::Path::new(path);
    let folder = p.parent().unwrap_or(p);
    let title = folder
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "SnapDrop".to_string());

    let wide_class: Vec<u16> = "CabinetWClass"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let find_folder_window = || -> Option<windows::Win32::Foundation::HWND> {
        unsafe {
            let mut after = windows::Win32::Foundation::HWND::default();
            loop {
                let h = FindWindowExW(
                    None,
                    Some(after),
                    PCWSTR(wide_class.as_ptr()),
                    PCWSTR::null(),
                )
                .ok()?;
                if h.is_invalid() || h == after {
                    return None;
                }
                let mut buf = [0u16; 512];
                let len = GetWindowTextW(h, &mut buf);
                let t = String::from_utf16_lossy(&buf[..len as usize]);
                if t.starts_with(&title) {
                    return Some(h);
                }
                after = h;
            }
        }
    };

    let activate = |h: windows::Win32::Foundation::HWND| unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        SwitchToThisWindow(h, true);
        let _ = SetForegroundWindow(h);
    };

    if let Some(h) = find_folder_window() {
        activate(h);
        return Ok(());
    }

    // No existing window on that folder — open one (selecting the file), then
    // bring the new window to the foreground (Explorer can open in the
    // background when launched from another process).
    let args = format!("/select,\"{}\"", path);
    let wide_args: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::w!("open"),
            windows::core::w!("explorer.exe"),
            PCWSTR(wide_args.as_ptr()),
            None,
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
    // Poll briefly for the new Explorer window and force it to the front.
    for _ in 0..15 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Some(h) = find_folder_window() {
            activate(h);
            break;
        }
    }
    Ok(())
}
