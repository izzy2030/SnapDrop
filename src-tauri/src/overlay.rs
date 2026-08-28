//! Native Win32 capture overlay: a full-virtual-screen layered window that
//! dims the desktop, draws a selection rectangle, and runs a nested message
//! loop until the user completes or cancels the selection.
//!
//! All coordinates are physical pixels in virtual-screen space.

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_ESCAPE, VK_RETURN,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    KillTimer, LoadCursorW, PostMessageW, RegisterClassExW, SetForegroundWindow, SetTimer,
    ShowWindow, TranslateMessage, UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, IDC_CROSS, MSG,
    SW_SHOW, ULW_ALPHA, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_NCHITTEST, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::monitors::{self, MonitorInfo};

/// True while the overlay's nested message loop is running (set on the main
/// thread just before the loop; cleared after). Lets the global Esc hotkey
/// dismiss the overlay even when the overlay window has no keyboard focus.
pub static RUNNING: AtomicBool = AtomicBool::new(false);

/// Snapshot of the "annotate before showing thumbnail" setting, used to label
/// the Ctrl modifier correctly in the overlay hints ("skip editor" vs
/// "annotate"). Set once per capture before the overlay starts.
static EDITOR_ENABLED: AtomicBool = AtomicBool::new(true);

/// The configured delay duration in seconds; held-Shift during the selection
/// arms the delayed capture (open menus/tooltips before the shot fires).
static DELAY_SECS: AtomicU32 = AtomicU32::new(0);

const CLASS_NAME: &str = "SnapDropOverlayClass";
const WM_OVERLAY_DONE: u32 = WM_APP + 1;
const TIMER_COUNTDOWN: usize = 1;
const HTTRANSPARENT: i32 = -1;
const DIM_ALPHA: u32 = 96;
const BORDER_COLOR: u32 = 0xFF_2F_7B_F6; // premultiplied ARGB
const GUIDE_COLOR: u32 = 0x8C_FF_FF_FF; // premultiplied white, alpha 140
const MIN_SELECTION: i32 = 4;
const BORDER_T: usize = 2;
const RESIZE_TOL: i32 = 8; // px within an edge to start resizing
const CLICK_MOVE: i32 = 3; // px of movement before a press counts as a drag
const HANDLE: i32 = 6; // corner-handle half-size when drawing
const EDGE_LEFT: u8 = 1;
const EDGE_RIGHT: u8 = 2;
const EDGE_TOP: u8 = 4;
const EDGE_BOTTOM: u8 = 8;

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub rect: RECT,
    /// True when Ctrl was held while the selection was being dragged. The
    /// capture flow flips the editor decision from the current setting: with
    /// the editor enabled Ctrl skips it, with it disabled Ctrl opens it.
    pub ctrl_held: bool,
    /// True when Shift was held at selection time — arms the delayed capture.
    pub shift_held: bool,
}

/// How the current mouse press is being interpreted inside the overlay.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OverlayMode {
    /// No press in progress.
    None,
    /// Drawing a fresh selection box (normal capture).
    Select,
    /// Dragging the pre-positioned last-area rectangle to move it.
    Move,
    /// Dragging an edge/corner of the last-area rectangle to resize it.
    Resize,
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
    /// Seconds to wait after the selection is released before capturing
    /// (0 = capture immediately).
    delayed_secs: u32,
    /// True while the countdown runs: the overlay is click-through and shows
    /// only a ghost outline + remaining seconds so the user can interact with
    /// the app below (e.g. open a dropdown) before the shot fires.
    counting: bool,
    countdown: i32,
    /// The finalized selection (virtual coords) captured at mouse-up.
    sel_rect: RECT,
    sel_ctrl: bool,
    /// Pre-positioned rectangle for "last area" mode (virtual coords).
    /// None for a normal capture, or cleared once the user starts a fresh
    /// drag outside the remembered area.
    initial: Option<RECT>,
    /// Interaction mode for the current press.
    mode: OverlayMode,
    /// Cursor position when the current press began (click vs drag test).
    press_start: POINT,
    /// The rectangle at press time — the base for move/resize deltas.
    press_rect: RECT,
    /// Cursor-to-rect offset while moving (keeps the grab under the cursor).
    grab_offset: POINT,
    /// Edges under the cursor while resizing (EDGE_* bitmask).
    resize_edge: u8,
    /// Modifier latches for the current press. Sampling GetAsyncKeyState only
    /// at mouse-up races with the key-up: the raw-input thread applies a key
    /// release immediately, while the mouse-up message waits in the queue
    /// behind drag redraws — so "hold Ctrl from before the drag" was often
    /// read as up. The latch ORs the state across the whole press.
    press_ctrl: bool,
    press_shift: bool,
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
            delayed_secs: 0,
            counting: false,
            countdown: 0,
            sel_rect: RECT::default(),
            sel_ctrl: false,
            initial: None,
            mode: OverlayMode::None,
            press_start: POINT::default(),
            press_rect: RECT::default(),
            grab_offset: POINT::default(),
            resize_edge: 0,
            press_ctrl: false,
            press_shift: false,
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
                let _ = SetCapture(hwnd);
                st.press_ctrl = is_ctrl_down();
                st.press_shift = is_shift_down();
                if let Some(rect) = st.initial {
                    let edge = hit_edge(&rect, pos, RESIZE_TOL);
                    if edge != 0 || point_in_rect(&rect, pos) {
                        // Adjust the remembered area: edges resize, interior moves.
                        st.press_start = pos;
                        st.press_rect = rect;
                        st.sel_rect = rect;
                        if edge != 0 {
                            st.mode = OverlayMode::Resize;
                            st.resize_edge = edge;
                            crate::debuglog::log(&format!(
                                "overlay: last-area resize edge={edge}"
                            ));
                        } else {
                            st.mode = OverlayMode::Move;
                            st.grab_offset = POINT {
                                x: pos.x - rect.left,
                                y: pos.y - rect.top,
                            };
                            crate::debuglog::log("overlay: last-area move");
                        }
                    } else {
                        // Outside the remembered area: start a fresh selection.
                        st.initial = None;
                        st.mode = OverlayMode::Select;
                        st.start = Some(pos);
                        st.cur = pos;
                        crate::debuglog::log("overlay: last-area -> fresh selection");
                    }
                } else {
                    st.mode = OverlayMode::Select;
                    st.start = Some(pos);
                    st.cur = pos;
                }
            }
            redraw();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let pos = cursor_pos();
            {
                let mut st = lock_state();
                match st.mode {
                    OverlayMode::Move => {
                        let virt = monitors::virtual_screen();
                        let b = st.press_rect;
                        let rw = b.right - b.left;
                        let rh = b.bottom - b.top;
                        let dx = pos.x - st.press_start.x;
                        let dy = pos.y - st.press_start.y;
                        let left = (b.left + dx).clamp(virt.left, (virt.right - rw).max(virt.left));
                        let top = (b.top + dy).clamp(virt.top, (virt.bottom - rh).max(virt.top));
                        st.sel_rect = RECT {
                            left,
                            top,
                            right: left + rw,
                            bottom: top + rh,
                        };
                    }
                    OverlayMode::Resize => {
                        let virt = monitors::virtual_screen();
                        let b = st.press_rect;
                        let mut left = b.left;
                        let mut right = b.right;
                        let mut top = b.top;
                        let mut bottom = b.bottom;
                        if st.resize_edge & EDGE_LEFT != 0 {
                            left = pos.x.clamp(virt.left, (b.right - MIN_SELECTION).max(virt.left));
                        }
                        if st.resize_edge & EDGE_RIGHT != 0 {
                            right = pos.x.clamp((b.left + MIN_SELECTION).min(virt.right), virt.right);
                        }
                        if st.resize_edge & EDGE_TOP != 0 {
                            top = pos.y.clamp(virt.top, (b.bottom - MIN_SELECTION).max(virt.top));
                        }
                        if st.resize_edge & EDGE_BOTTOM != 0 {
                            bottom = pos.y.clamp((b.top + MIN_SELECTION).min(virt.bottom), virt.bottom);
                        }
                        st.sel_rect = RECT {
                            left,
                            top,
                            right,
                            bottom,
                        };
                    }
                    OverlayMode::Select => {
                        if st.start.is_some() {
                            st.cur = pos;
                            // Latch modifiers held at any point of the drag.
                            st.press_ctrl |= is_ctrl_down();
                            st.press_shift |= is_shift_down();
                        }
                    }
                    OverlayMode::None => {}
                }
            }
            redraw();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            let pos = cursor_pos();
            {
                let mut st = lock_state();
                match st.mode {
                    OverlayMode::Move | OverlayMode::Resize => {
                        let moved = (pos.x - st.press_start.x).abs() >= CLICK_MOVE
                            || (pos.y - st.press_start.y).abs() >= CLICK_MOVE;
                        if moved {
                            // Adjusted — persist the new rectangle (the next
                            // press would otherwise reset it to the original)
                            // and stay in last-area mode for more tweaks.
                            crate::debuglog::log("overlay: last-area adjusted (move/resize)");
                            st.initial = Some(st.sel_rect);
                            st.mode = OverlayMode::None;
                            st.press_start = POINT::default();
                        } else {
                            // A click on the remembered area captures it now.
                            crate::debuglog::log("overlay: last-area confirmed (click)");
                            let rect = st.sel_rect;
                            let ctrl_held = is_ctrl_down() || st.press_ctrl;
                            let shift_held = is_shift_down() || st.press_shift;
                            st.press_ctrl = false;
                            st.press_shift = false;
                            st.mode = OverlayMode::None;
                            start_or_finish(&mut st, hwnd, rect, ctrl_held, shift_held);
                        }
                    }
                    OverlayMode::Select => {
                        let sel = st.start.map(|s| make_selection(s, st.cur));
                        let ok = sel
                            .map(|r| {
                                monitors::rect_width(&r) >= MIN_SELECTION
                                    && monitors::rect_height(&r) >= MIN_SELECTION
                            })
                            .unwrap_or(false);
                        let ctrl_held = is_ctrl_down() || st.press_ctrl;
                        let shift_held = is_shift_down() || st.press_shift;
                        st.press_ctrl = false;
                        st.press_shift = false;
                        crate::debuglog::log(&format!(
                            "overlay: mouse up, selection_ok={} ctrl_held={} shift_held={}",
                            ok, ctrl_held, shift_held
                        ));
                        let rect = sel.unwrap_or_default();
                        st.mode = OverlayMode::None;
                        if ok {
                            start_or_finish(&mut st, hwnd, rect, ctrl_held, shift_held);
                        } else {
                            st.done = true;
                            st.result = Some(None);
                            wake_loop(hwnd);
                        }
                    }
                    OverlayMode::None => {}
                }
            }
            redraw();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 as usize == TIMER_COUNTDOWN => {
            {
                let mut st = lock_state();
                if st.counting {
                    st.countdown -= 1;
                    if st.countdown <= 0 {
                        st.counting = false;
                        let _ = KillTimer(Some(hwnd), TIMER_COUNTDOWN);
                        st.done = true;
                        st.result = Some(Some(Selection {
                            rect: st.sel_rect,
                            ctrl_held: st.sel_ctrl,
                            shift_held: true, // countdown only arms on Shift
                        }));
                        crate::debuglog::log("overlay: countdown finished -> capture");
                        wake_loop(hwnd);
                    } else {
                        crate::debuglog::log(&format!("overlay: countdown {}", st.countdown));
                    }
                }
            }
            // NOTE: the state guard must be dropped before redraw() — it locks
            // the same mutex and std::sync::Mutex is not reentrant (deadlock).
            redraw();
            LRESULT(0)
        }
        // Click-through while the countdown runs: mouse events fall through to
        // the window(s) below so the user can keep interacting with the app.
        WM_NCHITTEST => {
            if lock_state().counting {
                LRESULT(HTTRANSPARENT as isize)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_KEYDOWN if wparam.0 as u32 == VK_ESCAPE.0 as u32 => {
            cancel();
            LRESULT(0)
        }
        // Enter confirms the pre-positioned last area (equivalent to a click).
        WM_KEYDOWN if wparam.0 as u32 == VK_RETURN.0 as u32 => {
            {
                let mut st = lock_state();
                if st.initial.is_some() && !st.counting && st.mode == OverlayMode::None {
                    crate::debuglog::log("overlay: last-area confirmed (Enter)");
                    let rect = st.sel_rect;
                    let ctrl_held = is_ctrl_down();
                    let shift_held = is_shift_down();
                    start_or_finish(&mut st, hwnd, rect, ctrl_held, shift_held);
                }
            }
            redraw();
            LRESULT(0)
        }
        // Refresh the live "skip editor" cue when Ctrl is pressed/released while
        // the overlay holds focus (mouse moves refresh it even without focus).
        WM_KEYDOWN | WM_KEYUP if wparam.0 as u32 == VK_CONTROL.0 as u32 => {
            redraw();
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
        if st.counting {
            st.counting = false;
            unsafe {
                let _ = KillTimer(Some(st.hwnd), TIMER_COUNTDOWN);
            }
        }
        wake_loop(st.hwnd);
    }
}

/// Whether the Ctrl key is physically held down right now. Uses
/// GetAsyncKeyState so it works even when the overlay can't take keyboard
/// focus (a background app can't always steal the foreground).
fn is_ctrl_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16) & 0x8000 != 0 }
}

fn is_shift_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16) & 0x8000 != 0 }
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

fn point_in_rect(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

/// Which edges of `r` are within `tol` px of `p` (EDGE_* bitmask). A press
/// near a border resizes; a press in the interior moves.
fn hit_edge(r: &RECT, p: POINT, tol: i32) -> u8 {
    let mut e = 0u8;
    if (p.x - r.left).abs() <= tol {
        e |= EDGE_LEFT;
    }
    if (p.x - r.right).abs() <= tol {
        e |= EDGE_RIGHT;
    }
    if (p.y - r.top).abs() <= tol {
        e |= EDGE_TOP;
    }
    if (p.y - r.bottom).abs() <= tol {
        e |= EDGE_BOTTOM;
    }
    e
}

/// Clamp a remembered area into the current virtual screen; None if it no
/// longer intersects (e.g. the monitor it was on was unplugged).
fn clamp_initial(rect: Option<RECT>, virt: &RECT) -> Option<RECT> {
    let r = rect?;
    let left = r.left.max(virt.left);
    let top = r.top.max(virt.top);
    let right = r.right.min(virt.right);
    let bottom = r.bottom.min(virt.bottom);
    if right - left < MIN_SELECTION || bottom - top < MIN_SELECTION {
        return None;
    }
    Some(RECT {
        left,
        top,
        right,
        bottom,
    })
}

/// Common completion for a finalized rectangle: with Shift held and a delay
/// configured, start the click-through countdown; otherwise complete the
/// overlay with the selection.
fn start_or_finish(st: &mut OverlayState, hwnd: HWND, rect: RECT, ctrl: bool, shift: bool) {
    if shift && st.delayed_secs > 0 {
        st.sel_rect = rect;
        st.sel_ctrl = ctrl;
        st.start = None;
        st.counting = true;
        st.countdown = st.delayed_secs as i32;
        let _ = unsafe { SetFocus(None) };
        let _ = unsafe { SetTimer(Some(hwnd), TIMER_COUNTDOWN, 1000, None) };
        crate::debuglog::log(&format!(
            "overlay: countdown started ({}s, shift-held)",
            st.delayed_secs
        ));
    } else {
        st.done = true;
        st.result = Some(Some(Selection {
            rect,
            ctrl_held: ctrl,
            shift_held: shift,
        }));
        wake_loop(hwnd);
    }
}

/// Run the capture overlay. Returns the selection (virtual-screen coords) or None if cancelled.
/// `editor_enabled` is the current "annotate before showing thumbnail" setting;
/// the overlay uses it to label the Ctrl modifier correctly. `delay_secs` is
/// the configured delayed-capture duration: holding Shift while selecting
/// arms the countdown (click-through + ghost outline, Esc cancels) so the
/// user can open menus/tooltips before the shot fires.
pub fn run(editor_enabled: bool, delay_secs: u32) -> Option<Selection> {
    run_impl(editor_enabled, delay_secs, None)
}

/// "Last area" variant: opens the overlay with the previous capture region
/// pre-positioned. Click the area to re-capture it instantly, drag inside to
/// move it, drag an edge to resize it, or drag elsewhere for a new selection.
pub fn run_last_area(editor_enabled: bool, delay_secs: u32, initial: RECT) -> Option<Selection> {
    run_impl(editor_enabled, delay_secs, Some(initial))
}

fn run_impl(editor_enabled: bool, delay_secs: u32, initial: Option<RECT>) -> Option<Selection> {
    EDITOR_ENABLED.store(editor_enabled, Ordering::SeqCst);
    DELAY_SECS.store(delay_secs, Ordering::SeqCst);
    {
        let mut st = lock_state();
        st.delayed_secs = delay_secs;
        st.counting = false;
        st.countdown = delay_secs as i32;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_inner(initial)));
    match result {
        Ok(sel) => sel,
        Err(payload) => {
            log::error!("overlay::run panicked: {}", crate::panic_message(&payload));
            None
        }
    }
}

fn run_inner(initial_rect: Option<RECT>) -> Option<Selection> {
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
        st.counting = false;
        st.countdown = st.delayed_secs as i32;
        st.initial = clamp_initial(initial_rect, &virt);
        st.mode = OverlayMode::None;
        st.press_start = POINT::default();
        st.press_rect = RECT::default();
        st.grab_offset = POINT::default();
        st.resize_edge = 0;
        st.press_ctrl = false;
        st.press_shift = false;
        if let Some(r) = st.initial {
            st.sel_rect = r;
            crate::debuglog::log(&format!("overlay: last-area mode rect={:?}", r));
        }
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
        let _ = KillTimer(Some(hwnd), TIMER_COUNTDOWN);

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

    if st.counting {
        // Delayed-capture countdown: fully transparent (no dim) so the user
        // sees and can interact with the app below. Show only a ghost outline
        // of the selection, the remaining seconds in the center, and a hint.
        buf.fill(0);
        let rect = st.sel_rect;
        let bx0 = (rect.left - virt.left).max(0) as usize;
        let by0 = (rect.top - virt.top).max(0) as usize;
        let bx1 = (rect.right - virt.left).min(w as i32) as usize;
        let by1 = (rect.bottom - virt.top).min(h as i32) as usize;
        stroke_rect(buf, w, h, bx0, by0, bx1, by1, BORDER_COLOR, BORDER_T);
        let cx = (rect.left + rect.right) / 2;
        let cy = (rect.top + rect.bottom) / 2;
        draw_pill_text(
            buf,
            w,
            h,
            &virt,
            cx,
            cy - 24,
            &format!("{}  •  Esc to cancel", st.countdown.max(0)),
            44,
        );
        draw_pill_text(
            buf,
            w,
            h,
            &virt,
            virt.left + w as i32 / 2,
            virt.top + 28,
            "Capturing selection…",
            15,
        );
    } else if st.initial.is_some() && st.mode != OverlayMode::Select {
        // Last-area mode: the remembered rectangle, editable, with handles.
        let rect = st.sel_rect;

        // The box is kept lightly tinted rather than fully transparent: layered
        // windows only receive mouse input on non-transparent pixels, and the
        // box must stay clickable for click-to-capture / move / resize. The
        // tint is lighter than the dim, so the box reads as a highlighted area.
        const BOX_TINT: u32 = 0x28 << 24; // premultiplied black, alpha 40
        let x0 = (rect.left.max(virt.left)) as usize;
        let y0 = (rect.top.max(virt.top)) as usize;
        let x1 = (rect.right.min(virt.right)) as usize;
        let y1 = (rect.bottom.min(virt.bottom)) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                buf[(y - virt.top as usize) * w + (x - virt.left as usize)] = BOX_TINT;
            }
        }
        // Border around the hole.
        let bx0 = (rect.left - virt.left).max(0) as usize;
        let by0 = (rect.top - virt.top).max(0) as usize;
        let bx1 = (rect.right - virt.left).min(w as i32) as usize;
        let by1 = (rect.bottom - virt.top).min(h as i32) as usize;
        stroke_rect(buf, w, h, bx0, by0, bx1, by1, BORDER_COLOR, BORDER_T);
        draw_handles(buf, w, h, &virt, &rect);

        // Dimension readout next to the rectangle.
        let scale = cursor_monitor_scale(&st);
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
            rect.left + rw / 2,
            y_pos,
            &text,
            15,
        );

        // Top hint.
        draw_pill_text(
            buf,
            w,
            h,
            &virt,
            virt.left + w as i32 / 2,
            virt.top + 28,
            "Click to capture • drag inside = move • drag edge = resize • drag elsewhere = new selection • Esc cancels",
            14,
        );
    } else if let Some(start) = st.start {
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

        // Dimension readout, with a live "skip editor" cue while Ctrl is held.
        let rw = monitors::rect_width(&rect);
        let rh = monitors::rect_height(&rect);
        let mut text = format!(
            "{} × {}",
            (rw as f32 / scale).round() as i64,
            (rh as f32 / scale).round() as i64
        );
        if is_ctrl_down() {
            let cue = if EDITOR_ENABLED.load(Ordering::SeqCst) {
                "   •   Ctrl: skip editor"
            } else {
                "   •   Ctrl: annotate"
            };
            text.push_str(cue);
        }
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
            15,
        );
    } else {
        // Hint before the first click.
        let mut hint = if EDITOR_ENABLED.load(Ordering::SeqCst) {
            "Drag to select   •   Esc to cancel   •   Ctrl: skip editor".to_string()
        } else {
            "Drag to select   •   Esc to cancel   •   Ctrl: annotate".to_string()
        };
        let d = DELAY_SECS.load(Ordering::SeqCst);
        if d > 0 {
            hint.push_str(&format!("   •   Shift: {}s delay", d));
        }
        draw_pill_text(buf, w, h, &virt, virt.left + w as i32 / 2, virt.top + 28, &hint, 15);
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

/// Draw small filled squares at the four corners of a rectangle (resize
/// handles) in buffer coordinates.
fn draw_handles(buf: &mut [u32], w: usize, h: usize, virt: &RECT, rect: &RECT) {
    let corners = [
        (rect.left, rect.top),
        (rect.right, rect.top),
        (rect.left, rect.bottom),
        (rect.right, rect.bottom),
    ];
    for (cx, cy) in corners {
        let x0 = (cx - virt.left) - HANDLE;
        let y0 = (cy - virt.top) - HANDLE;
        for dy in 0..(HANDLE * 2) as i64 {
            for dx in 0..(HANDLE * 2) as i64 {
                let x = x0 as i64 + dx;
                let y = y0 as i64 + dy;
                if x >= 0 && y >= 0 && x < w as i64 && y < h as i64 {
                    buf[y as usize * w + x as usize] = BORDER_COLOR;
                }
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
    font_h: i32,
) {
    let font_h = font_h;
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
