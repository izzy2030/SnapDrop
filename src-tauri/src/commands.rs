//! Tauri IPC commands.

use std::fs;
use std::path::Path;

use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::HWND;

use crate::{clipboard, dragdrop, filename, history, hotkey, settings, thumbnail};

#[tauri::command]
pub fn get_settings(app: AppHandle) -> settings::Settings {
    let mut s = settings::get(&app);
    s.screenshot_dir = filename::expand_dir(&s.screenshot_dir)
        .to_string_lossy()
        .to_string();
    s
}

#[tauri::command]
pub fn update_settings(app: AppHandle, settings: settings::Settings) -> Result<(), String> {
    let old = settings::get(&app);

    // Validate the screenshot directory before accepting changes.
    let dir = filename::expand_dir(&settings.screenshot_dir);
    if let Err(e) = filename::ensure_dir_writable(&dir) {
        return Err(e);
    }

    // Re-register hotkey if it changed (conflict → error, keep old).
    if settings.hotkey != old.hotkey {
        if let Err(e) = hotkey::apply_settings(&app, &settings.hotkey) {
            return Err(e);
        }
    }

    // Autostart toggle.
    if settings.start_with_windows != old.start_with_windows {
        use tauri_plugin_autostart::ManagerExt;
        if settings.start_with_windows {
            let _ = app.autolaunch().enable();
        } else {
            let _ = app.autolaunch().disable();
        }
    }

    settings::save(&app, &settings)?;
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

/// Open the configured screenshot folder in Explorer (tray + Settings).
pub fn open_folder_inner(app: &AppHandle) -> Result<(), String> {
    let dir = settings::resolved_dir(&settings::get(app));
    show_folder_in_explorer(&dir.to_string_lossy(), None)
}

/// Build a COM `VARIANT` holding a 32-bit integer (used as an index).
fn variant_i4(v: i32) -> windows::Win32::System::Variant::VARIANT {
    use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0};
    let mut loc = VARIANT::default();
    unsafe {
        let mut inner = VARIANT_0_0::default();
        inner.vt = windows::Win32::System::Variant::VT_I4;
        core::ptr::write(&mut inner.Anonymous.lVal, v);
        let mut v0 = VARIANT_0::default();
        core::ptr::write(&mut v0.Anonymous, core::mem::ManuallyDrop::new(inner));
        core::ptr::write(&mut loc.Anonymous, v0);
    }
    loc
}

/// Read a property from an `IDispatch` by name, returning the raw `VARIANT`.
/// The caller owns the result and must free any BSTR inside it.
fn dispatch_get(
    disp: &windows::Win32::System::Com::IDispatch,
    name: &str,
) -> Option<windows::Win32::System::Variant::VARIANT> {
    use windows::Win32::System::Com::{DISPATCH_PROPERTYGET, DISPPARAMS, EXCEPINFO};
    use windows::Win32::System::Variant::VARIANT;

    let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut dispid: i32 = 0;
    unsafe {
        disp.GetIDsOfNames(
            &windows_core::GUID::zeroed(),
            &windows_core::PCWSTR(name_wide.as_ptr()),
            1,
            0,
            &mut dispid,
        )
        .ok()?;
        let params = DISPPARAMS {
            rgvarg: core::ptr::null_mut(),
            rgdispidNamedArgs: core::ptr::null_mut(),
            cArgs: 0,
            cNamedArgs: 0,
        };
        let mut result = VARIANT::default();
        let mut excep = EXCEPINFO::default();
        let mut uargerr: u32 = 0;
        disp.Invoke(
            dispid,
            &windows_core::GUID::zeroed(),
            0,
            DISPATCH_PROPERTYGET,
            &params,
            Some(&mut result),
            Some(&mut excep),
            Some(&mut uargerr),
        )
        .ok()?;
        Some(result)
    }
}

/// Extract a `String` from a `VT_BSTR` VARIANT (freeing the BSTR afterwards).
fn variant_bstr_take(v: &mut windows::Win32::System::Variant::VARIANT) -> Option<String> {
    use windows::Win32::System::Variant::VT_BSTR;
    unsafe {
        if v.Anonymous.Anonymous.vt != VT_BSTR {
            return None;
        }
        let s = String::from_utf16_lossy(&v.Anonymous.Anonymous.Anonymous.bstrVal);
        let _ = windows::Win32::Foundation::SysFreeString(&v.Anonymous.Anonymous.Anonymous.bstrVal);
        Some(s)
    }
}

/// Extract a window handle from a `VT_I4`/`VT_I8` VARIANT. Shell windows
/// report their `HWND` as `VT_I8` on 64-bit Windows.
fn variant_hwnd_take(v: &windows::Win32::System::Variant::VARIANT) -> Option<isize> {
    use windows::Win32::System::Variant::{VT_I4, VT_I8};
    unsafe {
        match v.Anonymous.Anonymous.vt {
            VT_I4 => Some(v.Anonymous.Anonymous.Anonymous.lVal as isize),
            VT_I8 => Some(v.Anonymous.Anonymous.Anonymous.llVal as isize),
            _ => None,
        }
    }
}

/// Open the folder in Explorer, **reusing an existing Explorer window that is
/// already showing that folder**.
///
/// Existing windows are found by enumerating the shell's open folder windows
/// and comparing their real `LocationURL` (case-insensitively) against the
/// target path — never by window title, because unrelated folders can share a
/// name (e.g. the screenshots folder and a repo checkout both called
/// "SnapDrop").
///
/// When `select_file` is given and the folder isn't open yet, the folder is
/// opened with that file selected via `SHOpenFolderAndSelectItems` — the
/// shell's own reveal API, which also reuses an already-open window (bringing
/// it to the front with the file selected).
///
/// All COM work runs on a dedicated thread with its own STA initialization so
/// the calling thread's COM state is never disturbed.
fn show_folder_in_explorer(path: &str, select_file: Option<&str>) -> Result<(), String> {
    let path = path.to_string();
    let select_file = select_file.map(str::to_string);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let result = open_in_explorer_com(&path, select_file.as_deref());
        unsafe { CoUninitialize() };
        let _ = tx.send(result);
    });
    rx.recv().map_err(|e| e.to_string())?
}

/// COM-side implementation of `show_folder_in_explorer` (runs on the STA
/// worker thread).
fn open_in_explorer_com(path: &str, select_file: Option<&str>) -> Result<(), String> {
    match select_file {
        // Reveal a file: the shell API reuses an open window on the folder
        // (bringing it to the front with the file selected) or opens a new one.
        Some(file) => open_folder_selecting_file(path, file),
        // Plain folder open: reuse an existing window if one is open;
        // otherwise open a new window.
        None => match find_open_explorer_window(path) {
            Some(hwnd) => {
                focus_window(hwnd);
                Ok(())
            }
            None => open_folder_plain(path),
        },
    }
}

/// Enumerate the shell's open folder windows and return the HWND of the first
/// one whose real location matches `path` (case-insensitive, trailing-slash
/// tolerant). Never matches by window title.
fn find_open_explorer_window(path: &str) -> Option<HWND> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::UI::Shell::{IShellWindows, ShellWindows};

    // Explorer registers its open-folder windows with a LocationURL of the
    // form `file:///C:/Users/...` (three slashes after the scheme).
    let target_uri = format!("file:///{}/", path.replace('\\', "/"));

    let shell = unsafe { CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL) }.ok()?;
    let count = unsafe { shell.Count() }.ok()?;
    for i in 0..count {
        let index = variant_i4(i);
        let Ok(item) = (unsafe { shell.Item(&index) }) else {
            continue;
        };
        let mut loc = dispatch_get(&item, "LocationURL");
        let Some(loc_str) = loc.as_mut().and_then(variant_bstr_take) else {
            continue;
        };
        if loc_str
            .trim_end_matches('/')
            .eq_ignore_ascii_case(target_uri.trim_end_matches('/'))
        {
            if let Some(hwnd) = dispatch_get(&item, "HWND").as_ref().and_then(variant_hwnd_take) {
                if hwnd != 0 {
                    return Some(HWND(hwnd as *mut _));
                }
            }
        }
    }
    None
}

/// Restore (if minimized) and bring a window to the foreground.
fn focus_window(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SwitchToThisWindow, SW_RESTORE,
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        if !SetForegroundWindow(hwnd).as_bool() {
            // The foreground-window lock can reject background callers (e.g.
            // from the tray icon); SwitchToThisWindow is the classic fallback.
            SwitchToThisWindow(hwnd, true);
        }
    }
}

/// Open the folder with `file` selected via `SHOpenFolderAndSelectItems`.
/// If a window on the folder is already open it is brought to the front with
/// the file selected; otherwise a new window opens with the file selected.
fn open_folder_selecting_file(path: &str, file: &str) -> Result<(), String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{SHOpenFolderAndSelectItems, SHParseDisplayName};

    unsafe {
        let mut pidl_folder: *mut ITEMIDLIST = std::ptr::null_mut();
        let mut sfgao: u32 = 0;
        let folder_wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        if SHParseDisplayName(
            windows::core::PCWSTR(folder_wide.as_ptr()),
            None,
            &mut pidl_folder,
            0,
            Some(&mut sfgao),
        )
        .is_err()
        {
            return open_folder_plain(path);
        }

        let mut pidl_file: *mut ITEMIDLIST = std::ptr::null_mut();
        let file_wide: Vec<u16> = file.encode_utf16().chain(std::iter::once(0)).collect();
        let file_ok = SHParseDisplayName(
            windows::core::PCWSTR(file_wide.as_ptr()),
            None,
            &mut pidl_file,
            0,
            Some(&mut sfgao),
        )
        .is_ok();

        if !file_ok {
            // File vanished between capture and reveal — just open the folder.
            if !pidl_folder.is_null() {
                CoTaskMemFree(Some(pidl_folder as *const _));
            }
            return open_folder_plain(path);
        }

        let result = SHOpenFolderAndSelectItems(pidl_folder, Some(&[pidl_file]), 0);

        if !pidl_folder.is_null() {
            CoTaskMemFree(Some(pidl_folder as *const _));
        }
        if !pidl_file.is_null() {
            CoTaskMemFree(Some(pidl_file as *const _));
        }

        if result.is_err() {
            return open_folder_plain(path);
        }
    }
    Ok(())
}

/// Open the folder in a plain Explorer window.
fn open_folder_plain(path: &str) -> Result<(), String> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
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
pub fn get_capture_preview(_app: AppHandle, path: String) -> Result<String, String> {
    use base64::Engine;
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    if let Ok(reader) = image::ImageReader::open(&path).and_then(|r| r.with_guessed_format()) {
        if let Ok(img) = reader.decode() {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
            for px in rgba.pixels() {
                bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
            if let Some(png) = filename::preview_png(&bgra, w, h, 256) {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
                return Ok(format!("data:image/png;base64,{b64}"));
            }
        }
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:image/png;base64,{b64}"))
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
pub fn get_latest_capture() -> Option<thumbnail::CapturedPayload> {
    thumbnail::get_latest_capture()
}

#[tauri::command]
pub fn debug_log(msg: String) {
    crate::debuglog::log(&format!("renderer: {msg}"));
}

#[tauri::command]
pub fn get_debug_log(app: AppHandle) -> Result<String, String> {
    crate::debuglog::read(&app)
}

#[tauri::command]
pub fn open_debug_log(app: AppHandle) -> Result<(), String> {
    crate::debuglog::open(&app)
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
    // Reveal the file inside its folder. If an Explorer window is already open
    // on that folder, reuse it (path-based match, never by title); otherwise
    // open the folder with the file selected.
    let p = std::path::Path::new(path);
    let folder = p.parent().unwrap_or(p);
    show_folder_in_explorer(&folder.to_string_lossy(), Some(path))
}

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn debug_enum_shell_windows() {
        // IShellWindows requires COM initialized on this thread (STA).
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
        };
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        use windows::Win32::UI::Shell::{IShellWindows, ShellWindows};
        let shell = unsafe { CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL) };
        println!("CoCreateInstance: {:?}", shell.is_ok());
        if let Ok(shell) = shell {
            let count = unsafe { shell.Count() };
            println!("Count: {:?}", count);
            if let Ok(n) = count {
                for i in 0..n {
                    let index = variant_i4(i);
                    let item = unsafe { shell.Item(&index) };
                    println!("  Item[{i}]: {:?}", item.is_ok());
                    if let Ok(item) = item {
                        if let Some(mut loc) = dispatch_get(&item, "LocationURL") {
                            let s = variant_bstr_take(&mut loc);
                            println!("    LocationURL: {:?}", s);
                        } else {
                            println!("    LocationURL: <dispatch_get failed>");
                        }
                        if let Some(hwnd) =
                            dispatch_get(&item, "HWND").as_ref().and_then(variant_hwnd_take)
                        {
                            println!("    HWND: {:#x}", hwnd);
                        } else {
                            println!("    HWND: <none>");
                        }
                    }
                }
            }
        }
        unsafe {
            CoUninitialize();
        }
    }
}
