//! Monitor enumeration in physical-pixel (virtual screen) coordinates.

use std::mem::size_of;

use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, HDC, HMONITOR, MONITORINFOEXW,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct MonitorInfo {
    pub hmonitor: HMONITOR,
    /// Monitor rect in virtual-screen (physical pixel) coordinates.
    pub rect: RECT,
    /// Work area (excludes taskbar) in the same coordinate space.
    pub work: RECT,
    #[allow(dead_code)]
    pub is_primary: bool,
    /// Device name like "\\.\DISPLAY1" (null-terminated wide string).
    pub device: [u16; 32],
    /// Scale factor (dpi / 96).
    pub scale: f32,
}

/// Enumerate all monitors. Rects are in virtual-screen coordinates (physical pixels).
pub fn enumerate() -> Vec<MonitorInfo> {
    let mut monitors: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_proc),
            LPARAM(&mut monitors as *mut Vec<MonitorInfo> as isize),
        );
    }
    monitors
}

unsafe extern "system" fn enum_proc(
    hmon: HMONITOR,
    hdc: HDC,
    rect: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    // `extern "system"` callback — a Rust panic here aborts the process ("panic
    // in a function that cannot unwind"). Contain any panic so enumeration
    // simply skips the offending monitor instead.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        enum_proc_inner(hmon, hdc, rect, data)
    }));
    match result {
        Ok(b) => b,
        Err(payload) => {
            log::error!("monitor enumeration panicked: {}", crate::panic_message(&payload));
            windows::core::BOOL(1)
        }
    }
}

unsafe fn enum_proc_inner(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> windows::core::BOOL {
    let monitors = &mut *(data.0 as *mut Vec<MonitorInfo>);
    let mut mi: MONITORINFOEXW = std::mem::zeroed();
    mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmon, &mut mi.monitorInfo).as_bool() {
        let mut dpi_x: u32 = 96;
        let mut dpi_y: u32 = 96;
        let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        monitors.push(MonitorInfo {
            hmonitor: hmon,
            rect: mi.monitorInfo.rcMonitor,
            work: mi.monitorInfo.rcWork,
            is_primary: (mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
            device: mi.szDevice,
            scale: dpi_x as f32 / 96.0,
        });
    }
    windows::core::BOOL(1)
}

/// Bounding rect of the entire virtual screen (all monitors).
pub fn virtual_screen() -> RECT {
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

/// Monitor containing the given virtual-screen point.
pub fn monitor_at_point(x: i32, y: i32) -> Option<MonitorInfo> {
    let pt = windows::Win32::Foundation::POINT { x, y };
    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    enumerate().into_iter().find(|m| m.hmonitor == hmon)
}

pub fn rect_width(r: &RECT) -> i32 {
    r.right - r.left
}

pub fn rect_height(r: &RECT) -> i32 {
    r.bottom - r.top
}

/// Intersection of two rects (empty if disjoint).
pub fn intersect(a: &RECT, b: &RECT) -> RECT {
    RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    }
}

pub fn is_empty(r: &RECT) -> bool {
    r.right <= r.left || r.bottom <= r.top
}
