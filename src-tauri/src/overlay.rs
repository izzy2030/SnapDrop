//! Native Win32 capture overlay: a full-virtual-screen layered window that
//! dims the desktop, draws a selection rectangle, and runs a nested message
//! loop until the user completes or cancels the selection.
//!
//! All coordinates are physical pixels in virtual-screen space.

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
    GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    FF_DONTCARE, FW_NORMAL, HBITMAP, HDC, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    LoadCursorW, PostMessageW, RegisterClassExW, SetForegroundWindow, ShowWindow, TranslateMessage,
    UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, IDC_CROSS, MSG, SW_SHOW, ULW_ALPHA, WM_APP,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::monitors::{self, MonitorInfo};

/// True while the overlay's nested message loop is running (set on the main
/// thread just before the loop; cleared after). Lets the global Esc hotkey
/// dismiss the overlay even when the overlay window has no keyboard focus.
pub static RUNNING: AtomicBool = AtomicBool::new(false);

const CLASS_NAME: &str = "SnapDropOverlayClass";
const WM_OVERLAY_DONE: u32 = WM_APP + 1;
const DIM_ALPHA: u32 = 96;
const BORDER_COLOR: u32 = 0xFF_2F_7B_F6; // premultiplied ARGB
const GUIDE_COLOR: u32 = 0x8C_FF_FF_FF; // premultiplied white, alpha 140
const MIN_SELECTION: i32 = 4;
const BORDER_T: usize = 2;

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub rect: RECT,
}

struct OverlayState {
    hwnd: HWND,
    width: i32,
    height: i32,
    start: Option<POINT>,
    cur: POINT,
    done: bool,
    result: Option<Option<Selection>>,
    mem_dc: HDC,
    bmp: HBITMAP,
    bits: *mut u8,
    monitors: Vec<MonitorInfo>,
}

// Only ever touched on the main thread (nested loop during capture).
unsafe impl Send for OverlayState {}

/// Lock the overlay state, recovering from a poisoned mutex if an earlier
/// panic was caught while the guard was held (see `overlay_wndproc`).
fn lock_state() -> MutexGuard<'static, OverlayState> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

fn state() -> &'static Mutex<OverlayState> {
    static STATE: OnceLock<Mutex<OverlayState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(OverlayState {
            hwnd: HWND::default(),
            width: 0,
            height: 0,
            start: None,
            cur: POINT::default(),
            done: false,
            result: None,
            mem_dc: HDC::default(),
            bmp: HBITMAP::default(),
            bits: std::ptr::null_mut(),
            monitors: Vec::new(),
        })
    })
}

/// Register the overlay window class (once). Keeps the wide class name alive.
fn register_class() -> Option<u16> {
    static CLASS: OnceLock<(u16, &'static [u16])> = OnceLock::new();
    let (atom, _) = CLASS.get_or_init(|| unsafe {
        let hinst = HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap().0);
        let wide: Vec<u16> = CLASS_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let wide = Box::leak(wide.into_boxed_slice());
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap(),
            lpszClassName: PCWSTR(wide.as_ptr()),
            ..Default::default()
        };
        let atom = RegisterClassExW(&wc);
        (atom, wide)
    });
    (*atom != 0).then_some(*atom)
}

unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // This proc is an `extern "system"` callback: a Rust panic here cannot
    // unwind across the FFI boundary and would abort the whole process with
    // "panic in a function that cannot unwind" (0xc0000409). Catch and contain
    // any panic, then cancel the overlay so the screen is never left dimmed.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        overlay_wndproc_inner(hwnd, msg, wparam, lparam)
    }));
    match result {
        Ok(lres) => lres,
        Err(payload) => {
            log::error!("overlay wndproc panicked: {}", crate::panic_message(&payload));
            cancel();
            LRESULT(0)
        }
    }
}

unsafe fn overlay_wndproc_inner(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            let pos = cursor_pos();
            {
                let mut st = lock_state();
                st.start = Some(pos);
                st.cur = pos;
                let _ = SetCapture(hwnd);
            }
            redraw();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let pos = cursor_pos();
            {
                let mut st = lock_state();
                if st.start.is_some() {
                    st.cur = pos;
                }
            }
            redraw();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            let sel = {
                let st = lock_state();
                st.start.map(|s| make_selection(s, st.cur))
            };
            let ok = sel
                .map(|r| {
                    monitors::rect_width(&r) >= MIN_SELECTION
                        && monitors::rect_height(&r) >= MIN_SELECTION
                })
                .unwrap_or(false);
            {
                let mut st = lock_state();
                st.done = true;
                st.result = Some(if ok {
                    Some(Selection { rect: sel.unwrap() })
                } else {
                    None
                });
            }
            wake_loop(hwnd);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u32 == VK_ESCAPE.0 as u32 => {
            cancel();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn cancel() {
    let mut st = lock_state();
    if !st.done {
        st.done = true;
        st.result = Some(None);
        wake_loop(st.hwnd);
    }
}

/// Dismiss the overlay from outside the window proc (e.g. the global Esc
/// hotkey handler). No-op when no overlay is active.
pub fn cancel_if_running() {
    if RUNNING.load(Ordering::SeqCst) {
        log::info!("overlay: cancel requested via global hotkey");
        cancel();
    }
}

fn wake_loop(hwnd: HWND) {
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_OVERLAY_DONE, WPARAM(0), LPARAM(0));
    }
}

fn cursor_pos() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    pt
}

fn make_selection(a: POINT, b: POINT) -> RECT {
    RECT {
        left: a.x.min(b.x),
        top: a.y.min(b.y),
        right: a.x.max(b.x),
        bottom: a.y.max(b.y),
    }
}

/// Run the capture overlay. Returns the selection (virtual-screen coords) or None if cancelled.
pub fn run() -> Option<Selection> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run_inner));
    match result {
        Ok(sel) => sel,
        Err(payload) => {
            log::error!("overlay::run panicked: {}", crate::panic_message(&payload));
            None
        }
    }
}

fn run_inner() -> Option<Selection> {
    let monitors = monitors::enumerate();
    if monitors.is_empty() {
        return None;
    }
    let virt = monitors::virtual_screen();
    let w = monitors::rect_width(&virt);
    let h = monitors::rect_height(&virt);
    if w <= 0 || h <= 0 {
        return None;
    }
    if register_class().is_none() {
        return None;
    }

    unsafe {
        let hinst = HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap().0);
        let wide_name: Vec<u16> = CLASS_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            PCWSTR(wide_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            virt.left,
            virt.top,
            w,
            h,
            None,
            None,
            Some(hinst),
            None,
        );
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                log::error!("overlay: CreateWindowExW failed: {e}");
                return None;
            }
        };

        let mem_dc = CreateCompatibleDC(None);
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [Default::default()],
        };
        let bmp = match CreateDIBSection(Some(mem_dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) => b,
            Err(e) => {
                log::error!("overlay: CreateDIBSection failed: {e}");
                let _ = DestroyWindow(hwnd);
                let _ = DeleteDC(mem_dc);
                return None;
            }
        };
        let _ = SelectObject(mem_dc, bmp.into());

        {
            let mut st = lock_state();
            st.hwnd = hwnd;
            st.width = w;
            st.height = h;
            st.start = None;
            st.cur = cursor_pos();
            st.done = false;
            st.result = None;
            st.mem_dc = mem_dc;
            st.bmp = bmp;
            st.bits = bits as *mut u8;
            st.monitors = monitors;
        }

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));

        redraw();

        // Nested message loop. Never post WM_QUIT — this is Tauri's main thread.
        // While it runs, the overlay is "active": the global Esc hotkey calls
        // `cancel_if_running` to dismiss it even without keyboard focus.
        RUNNING.store(true, Ordering::SeqCst);
        let mut msg = MSG::default();
        loop {
            let b = GetMessageW(&mut msg, None, 0, 0);
            if b.0 == 0 {
                break; // WM_QUIT (shouldn't happen)
            }
            if b.0 == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
            if lock_state().done {
                break;
            }
        }
        RUNNING.store(false, Ordering::SeqCst);

        let result = lock_state().result.take();
        let _ = DestroyWindow(hwnd);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        {
            let mut st = lock_state();
            st.bits = std::ptr::null_mut();
            st.mem_dc = HDC::default();
            st.bmp = HBITMAP::default();
        }

        result.flatten()
    }
}

fn redraw() {
    let st = lock_state();
    if st.bits.is_null() || st.width <= 0 || st.height <= 0 {
        return;
    }
    let w = st.width as usize;
    let h = st.height as usize;
    let buf = unsafe { std::slice::from_raw_parts_mut(st.bits as *mut u32, w * h) };
    let virt = monitors::virtual_screen();

    // Dim the whole desktop.
    buf.fill((DIM_ALPHA << 24) | 0x00_00_00);

    let scale = cursor_monitor_scale(&st);

    if let Some(start) = st.start {
        let rect = make_selection(start, st.cur);

        // Transparent hole.
        let x0 = (rect.left.max(virt.left)) as usize;
        let y0 = (rect.top.max(virt.top)) as usize;
        let x1 = (rect.right.min(virt.right)) as usize;
        let y1 = (rect.bottom.min(virt.bottom)) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                buf[(y - virt.top as usize) * w + (x - virt.left as usize)] = 0;
            }
        }
        // Border around the hole (in buffer coords).
        let bx0 = (rect.left - virt.left).max(0) as usize;
        let by0 = (rect.top - virt.top).max(0) as usize;
        let bx1 = (rect.right - virt.left).min(w as i32) as usize;
        let by1 = (rect.bottom - virt.top).min(h as i32) as usize;
        stroke_rect(buf, w, h, bx0, by0, bx1, by1, BORDER_COLOR, BORDER_T);

        // Crosshair guides through the cursor.
        let cx = (st.cur.x - virt.left) as usize;
        let cy = (st.cur.y - virt.top) as usize;
        if cx < w {
            for y in 0..h {
                buf[y * w + cx] = GUIDE_COLOR;
            }
        }
        if cy < h {
            for x in 0..w {
                buf[cy * w + x] = GUIDE_COLOR;
            }
        }

        // Dimension readout.
        let rw = monitors::rect_width(&rect);
        let rh = monitors::rect_height(&rect);
        let text = format!(
            "{} × {}",
            (rw as f32 / scale).round() as i64,
            (rh as f32 / scale).round() as i64
        );
        let y_pos = if rect.top - virt.top < 60 {
            rect.bottom + 10
        } else {
            rect.top - 40
        };
        draw_pill_text(
            buf,
            w,
            h,
            &virt,
            st.cur.x,
            y_pos,
            &text,
        );
    } else {
        // Hint before the first click.
        draw_pill_text(
            buf,
            w,
            h,
            &virt,
            virt.left + w as i32 / 2,
            virt.top + 28,
            "Drag to select   •   Esc to cancel",
        );
    }

    unsafe {
        let point = POINT {
            x: virt.left,
            y: virt.top,
        };
        let size = SIZE {
            cx: st.width,
            cy: st.height,
        };
        let src = POINT { x: 0, y: 0 };
        let bf = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            st.hwnd,
            None,
            Some(&point),
            Some(&size),
            Some(st.mem_dc),
            Some(&src),
            COLORREF(0),
            Some(&bf),
            ULW_ALPHA,
        );
    }
}

fn cursor_monitor_scale(st: &OverlayState) -> f32 {
    st.monitors
        .iter()
        .find(|m| {
            m.rect.left <= st.cur.x
                && st.cur.x < m.rect.right
                && m.rect.top <= st.cur.y
                && st.cur.y < m.rect.bottom
        })
        .map(|m| m.scale)
        .unwrap_or(1.0)
}

fn stroke_rect(
    buf: &mut [u32],
    w: usize,
    h: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    color: u32,
    t: usize,
) {
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for y in y0.saturating_sub(t)..y1.saturating_add(t) {
        if y >= h {
            continue;
        }
        for x in x0.saturating_sub(t)..x1.saturating_add(t) {
            if x >= w {
                continue;
            }
            let inside = x >= x0 && x < x1 && y >= y0 && y < y1;
            if !inside {
                buf[y * w + x] = color;
            }
        }
    }
}

/// Draw a dark rounded pill with white text, anchored horizontally on `anchor_x`
/// (virtual coords) at vertical position `y_pos` (virtual coords).
fn draw_pill_text(
    buf: &mut [u32],
    w: usize,
    h: usize,
    virt: &RECT,
    anchor_x: i32,
    y_pos: i32,
    text: &str,
) {
    let font_h = 15i32;
    unsafe {
        let mem_dc = CreateCompatibleDC(None);
        if mem_dc.is_invalid() {
            return;
        }
        let font = CreateFontW(
            -font_h,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
            windows::core::w!("Segoe UI"),
        );
        let old_font = SelectObject(mem_dc, font.into());

        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(mem_dc, &wide, &mut sz);
        let tw = (sz.cx as usize + 28).max(48);
        let th = (font_h as usize + 12).max(24);

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: tw as i32,
                biHeight: -(th as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [Default::default()],
        };
        let sbmp = match CreateDIBSection(Some(mem_dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) => b,
            Err(_) => {
                let _ = SelectObject(mem_dc, old_font);
                let _ = DeleteObject(font.into());
                let _ = DeleteDC(mem_dc);
                return;
            }
        };
        let old_bmp = SelectObject(mem_dc, sbmp.into());
        let _ = SetBkMode(mem_dc, TRANSPARENT);
        let _ = SetTextColor(mem_dc, COLORREF(0xFF_FF_FF));

        let mut rc = RECT {
            left: 0,
            top: 0,
            right: tw as i32,
            bottom: th as i32,
        };
        let mut wide_mut = wide;
        let _ = DrawTextW(
            mem_dc,
            &mut wide_mut,
            &mut rc,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );

        // Blend scratch into the main buffer: dark pill, then white text (luminance → alpha).
        let px = std::slice::from_raw_parts(bits as *const u32, tw * th);
        let pill_x = (anchor_x - virt.left - tw as i32 / 2) as i64;
        let pill_y = (y_pos - virt.top) as i64;
        let pill_y = pill_y.clamp(0, h as i64 - th as i64 - 2);
        let pill_x = pill_x.clamp(0, w as i64 - tw as i64);

        let radius = 10usize;
        for dy in 0..th {
            for dx in 0..tw {
                let lx = pill_x + dx as i64;
                let ly = pill_y + dy as i64;
                if lx < 0 || ly < 0 || lx >= w as i64 || ly >= h as i64 {
                    continue;
                }
                // Rounded corners (skip the four corner squares).
                let corner = dx < radius && dy < radius
                    || dx >= tw - radius && dy < radius
                    || dx < radius && dy >= th - radius
                    || dx >= tw - radius && dy >= th - radius;
                if corner {
                    continue;
                }
                let dst = &mut buf[ly as usize * w + lx as usize];
                let t = px[dy * tw + dx];
                let lum = ((t >> 16) & 0xFF).max((t >> 8) & 0xFF).max(t & 0xFF) as u32;
                if lum > 0 {
                    // White text, premultiplied by luminance.
                    let sa = lum;
                    let da = (*dst >> 24) & 0xFF;
                    let out_a = sa + (da * (255 - sa) / 255);
                    let out_r = sa + ((*dst >> 16 & 0xFF) * (255 - sa) / 255);
                    let out_g = sa + ((*dst >> 8 & 0xFF) * (255 - sa) / 255);
                    let out_b = sa + ((*dst & 0xFF) * (255 - sa) / 255);
                    *dst = (out_a << 24) | (out_r << 16) | (out_g << 8) | out_b;
                } else {
                    // Pill background behind text pixels only (rough rounded pill).
                    let da = (*dst >> 24) & 0xFF;
                    let sa = 200u32;
                    let out_a = sa + (da * (255 - sa) / 255);
                    let out_r = (*dst >> 16 & 0xFF) * (255 - sa) / 255;
                    let out_g = (*dst >> 8 & 0xFF) * (255 - sa) / 255;
                    let out_b = (*dst & 0xFF) * (255 - sa) / 255;
                    *dst = (out_a << 24) | (out_r << 16) | (out_g << 8) | out_b;
                }
            }
        }
        let _ = SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(sbmp.into());
        let _ = SelectObject(mem_dc, old_font);
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(mem_dc);
    }
}
