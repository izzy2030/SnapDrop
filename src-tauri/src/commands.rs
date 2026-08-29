//! Tauri IPC commands.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::HWND;

use crate::{clipboard, dragdrop, filename, history, hotkey, ocr, settings, thumbnail};

/// Decoded-preview cache, keyed by capture path. Capture files are immutable
/// once written, so a path's preview never changes — decode each screenshot
/// at most once per process. (Without this, every history refresh re-read and
/// re-decoded every full-size screenshot, and those decodes ran synchronously
/// on the main/UI thread — stalling window drags, clicks, and deletions.)
fn preview_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn build_capture_preview(path: &str) -> Result<String, String> {
    // Videos: ask the Windows Shell thumbnailer (the same one Explorer uses)
    // for a real frame from the file. If that fails, return an empty preview
    // and the gallery renders the placeholder video card (play icon +
    // duration).
    if is_video_path(path) {
        return Ok(video_thumbnail_data_url(path).unwrap_or_default());
    }
    use base64::Engine;
    if let Ok(reader) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
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
    // Fallback: the whole file, base64-wrapped. (Read happens only here —
    // the decode path above never needs the raw bytes.)
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:image/png;base64,{b64}"))
}

/// Whether a path looks like a video file (mp4/mkv/webm/avi/mov).
fn is_video_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".mp4", ".mkv", ".webm", ".avi", ".mov"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Extract a video frame thumbnail via the Windows Shell thumbnailer
/// (`IShellItemImageFactory`, the same mechanism Explorer uses), converted to
/// a PNG data URL. Returns None on any failure — the gallery then falls back
/// to the placeholder video card.
fn video_thumbnail_data_url(path: &str) -> Option<String> {
    use std::ffi::c_void;
    use std::mem::size_of;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BI_RGB, BITMAP,
        BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY,
    };
    use windows::Win32::System::Com::IBindCtx;
    use windows::core::{Interface, PCWSTR};
    use base64::Engine;

    unsafe {
        // COM must be initialized on this (blocking-pool) thread for the
        // shell calls below.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // Normalize to backslashes: some shell APIs reject forward slashes.
        let norm = path.replace('/', "\\");
        let wide: Vec<u16> = norm.encode_utf16().chain(Some(0)).collect();
        let item: IShellItem = match SHCreateItemFromParsingName::<PCWSTR, Option<&IBindCtx>, _>(
            PCWSTR(wide.as_ptr()),
            None,
        ) {
            Ok(i) => i,
            Err(e) => {
                crate::debuglog::log(&format!("video_thumb: SHCreateItemFromParsingName failed: {e}"));
                CoUninitialize();
                return None;
            }
        };
        let factory: IShellItemImageFactory = match item.cast() {
            Ok(f) => f,
            Err(e) => {
                crate::debuglog::log(&format!("video_thumb: cast to IShellItemImageFactory failed: {e}"));
                CoUninitialize();
                return None;
            }
        };
        // Thumbnail-only (no icon overlay), fit within the 256 box without
        // cropping (SIIGBF_RESIZETOFIT = 0 is the default sizing).
        let hbm = match factory.GetImage(SIZE { cx: 256, cy: 256 }, SIIGBF_THUMBNAILONLY) {
            Ok(b) => b,
            Err(e) => {
                crate::debuglog::log(&format!("video_thumb: GetImage failed: {e}"));
                CoUninitialize();
                return None;
            }
        };

        let mut bmp = BITMAP::default();
        GetObjectW(
            hbm.into(),
            size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut BITMAP as *mut c_void),
        );
        let (w, h) = (bmp.bmWidth, bmp.bmHeight);
        if w <= 0 || h <= 0 {
            let _ = DeleteObject(hbm.into());
            CoUninitialize();
            return None;
        }

        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [Default::default()],
        };
        let dc = CreateCompatibleDC(None);
        let got = GetDIBits(
            dc,
            hbm,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteObject(hbm.into());
        let _ = DeleteDC(dc);
        CoUninitialize();
        if got == 0 {
            return None;
        }

        // The DIB is BGRA, top-down — exactly what preview_png expects.
        let png = crate::filename::preview_png(&buf, w as u32, h as u32, 256)?;
        Some(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        ))
    }
}

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

    // Same for the OCR hotkey.
    if settings.ocr_hotkey != old.ocr_hotkey {
        if let Err(e) = hotkey::apply_ocr_settings(&app, &settings.ocr_hotkey) {
            return Err(e);
        }
    }

    // Same for the last-area hotkey.
    if settings.last_area_hotkey != old.last_area_hotkey {
        if let Err(e) = hotkey::apply_last_area_settings(&app, &settings.last_area_hotkey) {
            return Err(e);
        }
    }

    // Same for the video-recording hotkey.
    if settings.video_hotkey != old.video_hotkey {
        if let Err(e) = hotkey::apply_video_settings(&app, &settings.video_hotkey) {
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
    if let Err(e) = fs::remove_file(&path) {
        crate::debuglog::log(&format!("history: delete failed for {path}: {e}"));
    }
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
pub fn start_drag(app: AppHandle, paths: Vec<String>) -> Result<dragdrop::DragOutcome, String> {
    // The watchdog compares pointerdown vs drag-start: a healthy page starts a
    // drag moments after the press, a wedged renderer never does.
    thumbnail::note_drag_started();
    let outcome = dragdrop::start_drag(&app, &paths)?;
    if outcome.moved {
        let s = settings::get(&app);
        if !s.keep_after_drag {
            // Move-drop with "keep file" off deletes every dragged file.
            for p in &paths {
                let _ = fs::remove_file(p);
                history::remove(&app, p);
            }
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
#[cfg(windows)]
pub async fn video_record_stop(
    app: AppHandle,
) -> Result<crate::video_recording::RecordingResult, String> {
    // Runs on the async runtime thread, NOT the main/UI thread. Stopping joins
    // the WGC capture thread, which finalizes the encoder by blocking on the
    // Media Foundation transcoder join. That whole chain would otherwise run on
    // the UI thread and freeze SnapDrop ("Not Responding").
    tauri::async_runtime::spawn_blocking(move || crate::video_recording::stop_recording(&app))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
#[cfg(windows)]
pub fn video_record_state(app: AppHandle) -> bool {
    use tauri::Manager;
    app.state::<Mutex<crate::video_recording::VideoRecorder>>()
        .lock()
        .unwrap()
        .is_recording()
}

/// Start recording the armed region (picked via Ctrl+Alt+V). Called by the
/// toolbar's Rec button. Runs off the main thread — `start_recording` spins
/// up the WGC capture session.
#[tauri::command]
#[cfg(windows)]
pub async fn video_record_begin(app: AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pending = {
            let rec_state = app.state::<Mutex<crate::video_recording::VideoRecorder>>();
            let mut rec = rec_state.lock().unwrap();
            rec.take_pending()
        };
        let Some(p) = pending else {
            return Err("No region selected — press Ctrl+Alt+V first".to_string());
        };
        match crate::video_recording::start_recording(&app, p.region, p.path, 30) {
            Ok(path) => Ok(path),
            Err(e) => {
                // Start failed (e.g. the monitor was unplugged). The pending
                // slot is already consumed, so restore the main window the way
                // it was before the selection overlay — otherwise the app stays
                // hidden while nothing records.
                crate::toolbar::hide(&app);
                crate::toolbar::hide_border(&app);
                crate::capture_flow::restore_main_window(&app, p.was_visible, p.was_minimized);
                crate::debuglog::log(&format!("video: start failed, restored window: {e}"));
                Err(e)
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Cancel an armed (not yet started) recording: clears the pending region,
/// hides the toolbar, and restores the main window the way it was.
#[tauri::command]
#[cfg(windows)]
pub fn video_record_arm_cancel(app: AppHandle) -> Result<(), String> {
    let (was_visible, was_minimized) = {
        let rec_state = app.state::<Mutex<crate::video_recording::VideoRecorder>>();
        let mut rec = rec_state.lock().unwrap();
        match rec.take_pending() {
            Some(p) => (p.was_visible, p.was_minimized),
            None => (true, false),
        }
    };
    crate::toolbar::hide(&app);
    crate::toolbar::hide_border(&app);
    // If a recording is actually live (Rec clicked, then Cancel clicked in
    // the same instant), don't restore the app window back into view —
    // SnapDrop would film itself. Just drop the armed toolbar/border and
    // leave the live session alone.
    if crate::video_recording::is_recording_active() {
        crate::debuglog::log("video: cancel ignored (a recording is active)");
        return Ok(());
    }
    crate::capture_flow::restore_main_window(&app, was_visible, was_minimized);
    crate::debuglog::log("video: armed recording cancelled");
    Ok(())
}

/// Toggle the system-audio mute; returns the new state for the toolbar UI.
#[tauri::command]
#[cfg(windows)]
pub fn video_toggle_mute() -> bool {
    crate::toolbar::toggle_mute()
}

/// Current muted state (toolbar reads it on mount).
#[tauri::command]
#[cfg(windows)]
pub fn video_mute_state() -> bool {
    crate::toolbar::is_muted()
}

#[tauri::command]
pub fn capture_now(app: AppHandle) -> Result<(), String> {
    hotkey::trigger_capture(&app);
    Ok(())
}

#[tauri::command]
pub async fn get_capture_preview(_app: AppHandle, path: String) -> Result<String, String> {
    // Cache hit → instant, no decode. Cache misses are decoded via
    // spawn_blocking: `async` + blocking pool keeps the heavy PNG decode OFF
    // the main/UI thread. A synchronous decode of a full-size screenshot
    // takes tens to hundreds of ms, and the gallery fetches one per entry on
    // every window focus — blocking the main thread exactly when the user
    // presses the title bar, which made window drags feel "stuck then pulling".
    if let Some(hit) = preview_cache().lock().unwrap().get(&path) {
        return Ok(hit.clone());
    }
    let decode_path = path.clone();
    let result = tauri::async_runtime::spawn_blocking(move || build_capture_preview(&decode_path))
        .await
        .map_err(|e| e.to_string())?;
    if let Ok(url) = &result {
        // Don't cache empty results: a failed video thumbnail would otherwise be
        // stuck as an empty string forever and never retried.
        if !url.is_empty() {
            let mut cache = preview_cache().lock().unwrap();
            if cache.len() > 64 {
                cache.clear(); // captures are capped by max_history anyway
            }
            cache.insert(path, url.clone());
        }
    }
    result
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

/// OCR the pending editor capture and return the recognized text. Runs on a
/// worker thread — the recognition blocks for ~100ms+ and must never sit on
/// the main/UI thread.
#[tauri::command]
pub async fn ocr_pending_editor_image() -> Result<String, String> {
    let Some((bgra, w, h)) = crate::editor::pending_bgra() else {
        return Err("No pending capture to recognize".into());
    };
    tauri::async_runtime::spawn_blocking(move || ocr::recognize(&bgra, w, h))
        .await
        .map_err(|e| format!("OCR task failed: {e}"))?
}

/// Copy plain text to the clipboard (editor OCR result).
#[tauri::command]
pub fn copy_text(text: String) -> Result<(), String> {
    clipboard::set_text(&text)
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

/// Throttled input-liveness signal from the thumbnail renderer. Feeds the
/// watchdog's "ghost" detection (JS alive but input pipeline stuck).
#[tauri::command]
pub fn report_renderer_input() {
    thumbnail::report_renderer_input();
}

/// Immediate (unthrottled) `pointerdown` signal from the thumbnail renderer.
/// Lets the watchdog distinguish a plain hover from a click that never became
/// a drag — the exact ghost signature.
#[tauri::command]
pub fn report_renderer_pointerdown() {
    thumbnail::report_renderer_pointerdown();
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
        crate::debuglog::log("show_settings_inner: showing main window");
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
mod preview_tests {
    use super::*;

    /// Quantifies why the preview decode must stay off the main thread:
    /// one gallery refresh used to run this once per entry, synchronously on
    /// the UI thread. Run with `cargo test -- --nocapture` to see the timing.
    #[test]
    fn preview_decode_cost_and_cache() {
        // Synthesize a 2560x1440 screenshot-sized PNG.
        let dir = std::env::temp_dir().join("snapdrop_preview_test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("cost_test.png");
        let w = 2560u32;
        let h = 1440u32;
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                bgra.extend_from_slice(&[(x % 255) as u8, (y % 255) as u8, 128, 255]);
            }
        }
        let png = filename::encode_png(&bgra, w, h).unwrap();
        fs::write(&path, &png).unwrap();

        let path_str = path.to_string_lossy().to_string();
        let start = std::time::Instant::now();
        let first = build_capture_preview(&path_str).unwrap();
        let cold_ms = start.elapsed().as_millis();

        // Prime the cache, then measure a hit.
        preview_cache().lock().unwrap().insert(path_str.clone(), first.clone());
        let start = std::time::Instant::now();
        let cached = preview_cache().lock().unwrap().get(&path_str).unwrap().clone();
        let warm_us = start.elapsed().as_micros();

        assert_eq!(first, cached, "cache hit must return the same preview");
        println!(
            "preview decode of a {w}x{h} screenshot: {cold_ms}ms cold, {warm_us}us cached — a 10-entry refresh used to cost ~{}ms ON THE MAIN THREAD",
            cold_ms * 10
        );
        let _ = fs::remove_file(&path);
    }
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

#[cfg(test)]
mod video_thumb_tests {
    use super::*;

    /// Dev-machine smoke test: if any SnapDrop_video_*.mp4 exists in the
    /// default Pictures/SnapDrop folder, extract its frame via the Windows
    /// Shell thumbnailer and assert a PNG data URL comes back. Skips silently
    /// when no recording exists yet (e.g. CI or a fresh machine).
    #[test]
    fn video_thumbnail_from_real_recording() {
        let dir = std::env::var("USERPROFILE")
            .map(|p| std::path::Path::new(&p).join("Pictures").join("SnapDrop"))
            .unwrap_or_default();
        let mut found = false;
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with("SnapDrop_video_") && name.ends_with(".mp4") {
                    let url = video_thumbnail_data_url(p.to_string_lossy().as_ref());
                    assert!(url.is_some(), "shell thumbnail failed for {}", p.display());
                    let url = url.unwrap();
                    assert!(
                        url.starts_with("data:image/png;base64,"),
                        "unexpected preview payload"
                    );
                    assert!(url.len() > 100, "thumbnail suspiciously small");
                    found = true;
                    break;
                }
            }
        }
        if !found {
            eprintln!("skipping: no SnapDrop_video_*.mp4 in {:?}", dir);
        }
    }
}
