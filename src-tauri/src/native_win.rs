//! Minimal raw-Win32 window helpers that do NOT go through Tauri's
//! main-thread event loop. Used for operations (like close-to-tray hide)
//! that must succeed even if the main thread is busy or momentarily wedged.
//! Keep this module OS-only; it is a thin, dependency-free shim over user32.

#![allow(dead_code)]

use std::ffi::c_void;

type HWND = *mut c_void;

/// ShowWindow with SW_HIDE: hides the window directly via the Win32 API.
/// `hwnd` is the raw pointer from `tauri::window.hwnd()`. Safe to call from
/// any thread (user32 window functions are thread-safe at this level).
pub unsafe fn hide_window(hwnd: *mut c_void) {
    if hwnd.is_null() {
        return;
    }
    extern "system" {
        fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> i32;
    }
    const SW_HIDE: i32 = 0;
    unsafe {
        let _ = ShowWindow(hwnd as HWND, SW_HIDE);
    }
}

/// ShowWindow with SW_SHOW (no activation): brings a window back without
/// stealing focus. Equivalent to the app's restore_main_window path.
pub unsafe fn show_no_activate(hwnd: *mut c_void) {
    if hwnd.is_null() {
        return;
    }
    extern "system" {
        fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> i32;
    }
    const SW_SHOWNOACTIVATE: i32 = 4;
    unsafe {
        let _ = ShowWindow(hwnd as HWND, SW_SHOWNOACTIVATE);
    }
}

/// ShowWindow with SW_SHOWNORMAL and SetForegroundWindow: shows the window directly
/// via Win32 and activates it, bypassing any cached/stale Tao diffing state.
pub unsafe fn show_window_foreground(hwnd: *mut c_void) {
    if hwnd.is_null() {
        return;
    }
    extern "system" {
        fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> i32;
        fn SetForegroundWindow(hWnd: HWND) -> i32;
    }
    const SW_SHOWNORMAL: i32 = 1;
    unsafe {
        let _ = ShowWindow(hwnd as HWND, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd as HWND);
    }
}

/// IsWindowVisible: check whether the window is currently visible. Does not
/// touch the main-thread event loop.
pub unsafe fn is_visible(hwnd: *mut c_void) -> bool {
    if hwnd.is_null() {
        return false;
    }
    extern "system" {
        fn IsWindowVisible(hWnd: HWND) -> i32;
    }
    unsafe { IsWindowVisible(hwnd as HWND) != 0 }
}