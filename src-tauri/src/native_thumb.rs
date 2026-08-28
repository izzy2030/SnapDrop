//! Native Win32 thumbnail window — experiment branch (`fix/native-thumbnail`).
//!
//! Replaces the WebView2 thumbnail renderer with a plain Win32 window painted
//! with GDI. Motivation: WebView2 auto-suspends hidden windows and its resume
//! path is flaky (stale frame, dead click→drag processing), which produced the
//! recurring "ghost thumbnail". A native window has no suspend logic, so the
//! entire bug class is gone by construction.
//!
//! Behavior parity with the webview renderer (`ThumbnailApp.tsx`):
//! - stack of up to 10 captures; older ones peek out behind the current card
//! - left press → native OLE drag of the file (runs on the Tauri main thread,
//!   exactly like the webview path — `dragdrop::start_drag` polls for real
//!   cursor movement, so a plain click never starts a drag)
//! - double-click → open with the default app; Ctrl+click → reveal in Explorer
//! - plain click with more than one capture → expand into a list of captures
//! - "Not saved" badge for unsaved captures, filename caption on top
//! - auto-dismiss timer (`thumbnail_duration_secs`), Esc → `thumbnail::hide_all`

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::{Mutex, Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, PhysicalPosition, PhysicalSize};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, InvalidateRect,
    SelectObject, SetBkMode, SetStretchBltMode, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, FW_NORMAL, HALFTONE,
    HDC, HFONT, OUT_DEFAULT_PRECIS, SetTextColor, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, IsWindowVisible,
    PostMessageW, PostQuitMessage, RegisterClassExW, SetWindowPos, ShowWindow, TranslateMessage,
    CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST, MA_NOACTIVATE, MSG, SWP_NOACTIVATE,    SWP_NOMOVE, SWP_SHOWWINDOW, SW_HIDE, WM_APP, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_PAINT, WNDCLASSEXW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const WM_APP_CMD: u32 = WM_APP + 1;
const WM_APP_TIMER: u32 = WM_APP + 2;

/// Extra physical pixels below the card so older captures can peek out.
const PEEK_SPACE: i32 = 20;
/// Height of a row in the expanded list (physical px).
const ROW_H: i32 = 48;
/// Height of the filename caption bar (physical px).
const CAPTION_H: i32 = 26;
/// Window class name.
const CLASS_NAME: PCWSTR = w!("SnapDropNativeThumb");

// Card colors matching styles.css (COLORREF is 0x00BBGGRR).
const CARD_BG: COLORREF = COLORREF(0x002b_221e); // #1e222b
const PEEK_BG: COLORREF = COLORREF(0x0012_0f14); // darker sliver behind the card
const CAPTION_BG: COLORREF = COLORREF(0x001a_1310); // #10131a
const PLACEHOLDER_BG: COLORREF = COLORREF(0x003a_2f2a); // #2a2f3a
const BORDER: COLORREF = COLORREF(0x004a_3f3a); // rgba(255,255,255,.15) approx
const ROW_BG: COLORREF = COLORREF(0x0022_1a17); // #171a22
const TEXT: COLORREF = COLORREF(0x00ff_ffff);
const BADGE_BG: COLORREF = COLORREF(0x000b_9ef5); // #f59e0b
const BADGE_TEXT: COLORREF = COLORREF(0x002b_221e);

/// One capture to show. `png` is the raw preview PNG (payload without the
/// data-URL prefix).
pub struct PresentEntry {
    pub capture_id: u64,
    pub path: Option<String>,
    pub png: Vec<u8>,
    pub unsaved: bool,
}

enum Cmd {
    Present(Box<PresentEntry>, (u32, u32), Option<(i32, i32)>, u64),
    Hide,
    Show,
    DragOutcome {
        capture_id: u64,
        dropped: bool,
        moved: bool,
        hide_after_drop: bool,
    },
    Open(String),
    Reveal(String),
}

struct Entry {
    capture_id: u64,
    path: Option<String>,
    file_name: String,
    /// Decoded preview as top-down BGRA, ready for StretchDIBits.
    bits: Option<(Vec<u8>, u32, u32)>,
    unsaved: bool,
}

struct State {
    stack: Vec<Entry>, // stack[0] is the current card
    seen: HashSet<u64>,
    expanded: bool,
    card_w: i32,
    card_h: i32,
    pos: Option<(i32, i32)>,
    auto_hide_ms: u64,
    timer_gen: u64,
    ctrl_pending: bool,
    last_dbl_ms: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            stack: Vec::new(),
            seen: HashSet::new(),
            expanded: false,
            card_w: 320,
            card_h: 240,
            pos: None,
            auto_hide_ms: 0,
            timer_gen: 0,
            ctrl_pending: false,
            last_dbl_ms: 0,
        }
    }
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static HWND_SLOT: OnceLock<isize> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();
static INIT: Once = Once::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Rebuild an HWND from the isize stored in the static slot (HWND wraps a
/// pointer in windows 0.62, which is not Send).
fn hwnd_from(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// Stack insertion policy: dedupe by capture id, newest first, cap at 10.
fn stack_push(stack: &mut Vec<Entry>, seen: &mut HashSet<u64>, entry: Entry) -> bool {
    if entry.capture_id == 0 || entry.bits.is_none() || !seen.insert(entry.capture_id) {
        return false;
    }
    stack.insert(0, entry);
    stack.truncate(10);
    true
}

fn stack_remove(stack: &mut Vec<Entry>, seen: &mut HashSet<u64>, capture_id: u64) -> bool {
    let before = stack.len();
    stack.retain(|e| e.capture_id != capture_id);
    if stack.len() != before {
        seen.remove(&capture_id);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Public API (called from thumbnail.rs)
// ---------------------------------------------------------------------------

pub fn present(
    app: &AppHandle,
    entry: PresentEntry,
    size: PhysicalSize<u32>,
    pos: Option<PhysicalPosition<i32>>,
    auto_hide_ms: u64,
) {
    let _ = APP.set(app.clone());
    post(Cmd::Present(
        Box::new(entry),
        (size.width, size.height),
        pos.map(|p| (p.x, p.y)),
        auto_hide_ms,
    ));
}

pub fn hide() {
    post(Cmd::Hide);
}

pub fn show() {
    post(Cmd::Show);
}

pub fn is_visible() -> bool {
    match HWND_SLOT.get() {
        Some(&h) => unsafe { IsWindowVisible(hwnd_from(h)).as_bool() },
        None => false,
    }
}

fn post(cmd: Cmd) {
    let hwnd = ensure_window();
    if hwnd == 0 {
        return;
    }
    let raw = Box::into_raw(Box::new(cmd));
    unsafe {
        let _ = PostMessageW(Some(hwnd_from(hwnd)), WM_APP_CMD, WPARAM(0), LPARAM(raw as isize));
    }
}

fn ensure_window() -> isize {
    INIT.call_once(|| {
        let (tx, rx) = mpsc::channel::<isize>();
        std::thread::spawn(move || unsafe {
            let hwnd = create_window();
            let _ = tx.send(hwnd.0 as isize);
            if hwnd.0.is_null() {
                return;
            }
            let mut msg = MSG::default();
            loop {
                let b = GetMessageW(&mut msg, None, 0, 0);
                if b.0 == 0 || b.0 == -1 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        match rx.recv() {
            Ok(h) if h != 0 => {
                let _ = HWND_SLOT.set(h);
            }
            _ => crate::debuglog::log("native_thumb: window creation failed"),
        }
    });
    *HWND_SLOT.get().unwrap_or(&0)
}

unsafe fn create_window() -> HWND {
    let hinst = HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap_or_default().0);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        lpfnWndProc: Some(thumb_wndproc),
        hInstance: hinst,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    RegisterClassExW(&wc);
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        CLASS_NAME,
        PCWSTR::null(),
        WS_POPUP,
        0,
        0,
        1,
        1,
        None,
        None,
        Some(hinst),
        None,
    )
    .unwrap_or_default();
    if hwnd.0.is_null() {
        return hwnd;
    }
    // Windows 11: rounded corners + drop shadow for free. Fails harmlessly on Win10.
    let pref = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &pref as *const _ as *const c_void,
        std::mem::size_of::<i32>() as u32,
    );
    hwnd
}

// ---------------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------------

unsafe extern "system" fn thumb_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_APP_TIMER => {
            handle_auto_hide(wparam);
            LRESULT(0)
        }
        WM_APP_CMD => {
            let cmd = Box::from_raw(lparam.0 as *mut Cmd);
            handle_cmd(hwnd, *cmd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_button_down(lparam);
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            on_double_click(hwnd, lparam);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            on_button_up(lparam);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn handle_cmd(hwnd: HWND, cmd: Cmd) {
    match cmd {
        Cmd::Present(entry, size, pos, auto_hide_ms) => {
            let entry = make_entry(*entry);
            let added;
            let gen;
            {
                let mut guard = state().lock().unwrap();
                let st = &mut *guard;
                added = stack_push(&mut st.stack, &mut st.seen, entry);
                if added {
                    st.expanded = false;
                    st.card_w = size.0 as i32;
                    st.card_h = size.1 as i32;
                    st.pos = pos;
                    st.auto_hide_ms = auto_hide_ms;
                    st.timer_gen += 1;
                    gen = st.timer_gen;
                } else {
                    gen = 0;
                }
            }
            if !added {
                // Duplicate capture id: keep showing what's already there.
                show_at(hwnd, current_window_size(), pos);
                return;
            }
            show_at(hwnd, current_window_size(), pos);
            if auto_hide_ms > 0 {
                spawn_auto_hide(hwnd, auto_hide_ms, gen);
            }
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
        Cmd::Hide => {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        Cmd::Show => {
            let pos = state().lock().unwrap().pos;
            show_at(hwnd, current_window_size(), pos);
        }
        Cmd::DragOutcome {
            capture_id,
            dropped,
            moved,
            hide_after_drop,
        } => {
            let mut guard = state().lock().unwrap();
            let st = &mut *guard;
            if moved && stack_remove(&mut st.stack, &mut st.seen, capture_id) {
                if st.stack.is_empty() {
                    drop(guard);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    return;
                }
                st.expanded = false;
                drop(guard);
                show_at(hwnd, current_window_size(), None);
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            } else if dropped && hide_after_drop {
                drop(guard);
                let _ = ShowWindow(hwnd, SW_HIDE);
            } else if !dropped && !moved {
                // Plain click: toggle the expanded list (if more than one).
                let suppress = now_ms().saturating_sub(st.last_dbl_ms) < 700;
                if !suppress && st.stack.len() > 1 {
                    st.expanded = !st.expanded;
                    let expanded = st.expanded;
                    drop(guard);
                    show_at(hwnd, current_window_size(), None);
                    crate::debuglog::log(&format!(
                        "native_thumb: expanded={expanded}"
                    ));
                }
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
        Cmd::Open(path) => {
            if let Some(handle) = APP.get().cloned() {
                let for_task = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    let _ = crate::commands::open_capture(for_task, path);
                });
            }
        }
        Cmd::Reveal(path) => {
            if let Some(handle) = APP.get().cloned() {
                let for_task = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    let _ = crate::commands::reveal_capture(for_task, path);
                });
            }
        }
    }
}

unsafe fn handle_auto_hide(wparam: WPARAM) {
    let Some(&hwnd) = HWND_SLOT.get() else {
        return;
    };
    let mut st = state().lock().unwrap();
    let expired = wparam.0 == st.timer_gen as usize
        && st.auto_hide_ms > 0
        && !st.expanded
        && !st.stack.is_empty()
        && IsWindowVisible(hwnd_from(hwnd)).as_bool();
    if !expired {
        return;
    }
    st.timer_gen = st.timer_gen.wrapping_add(1); // one-shot: don't refire
    drop(st);
    crate::debuglog::log("native_thumb: auto-dismiss timer fired");
    if let Some(handle) = APP.get().cloned() {
        let for_task = handle.clone();
        let _ = handle.run_on_main_thread(move || {
            let _ = crate::thumbnail::hide_all(&for_task);
        });
    }
}

unsafe fn spawn_auto_hide(hwnd: HWND, ms: u64, gen: u64) {
    let h = hwnd.0 as isize; // HWND wraps a pointer, which is not Send
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        let _ = PostMessageW(
            Some(hwnd_from(h)),
            WM_APP_TIMER,
            WPARAM(gen as usize),
            LPARAM(0),
        );
    });
}

fn current_window_size() -> (i32, i32) {
    let st = state().lock().unwrap();
    let rows = if st.expanded && st.stack.len() > 1 {
        (st.stack.len() as i32 - 1) * ROW_H
    } else {
        0
    };
    (st.card_w, st.card_h + PEEK_SPACE + rows)
}

unsafe fn show_at(hwnd: HWND, size: (i32, i32), pos: Option<(i32, i32)>) {
    let (w, h) = size;
    if let Some((x, y)) = pos {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    } else {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            w,
            h,
            SWP_NOMOVE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn hit_test(y: i32) -> usize {
    // Returns the stack index under the cursor: the card, or an expanded row.
    let st = state().lock().unwrap();
    if y < st.card_h {
        0
    } else if st.expanded {
        let idx = ((y - st.card_h) / ROW_H) as usize + 1;
        idx.min(st.stack.len().saturating_sub(1))
    } else {
        0
    }
}

fn entry_path(idx: usize) -> Option<String> {
    let st = state().lock().unwrap();
    st.stack.get(idx).and_then(|e| e.path.clone())
}

fn entry_id(idx: usize) -> u64 {
    let st = state().lock().unwrap();
    st.stack.get(idx).map(|e| e.capture_id).unwrap_or(0)
}

unsafe fn on_button_down(lparam: LPARAM) {
    let y = (lparam.0 >> 16) as u16 as i32; // client coords, physical
    let ctrl = key_down(VK_CONTROL.0 as i32)
        || key_down(VK_MENU.0 as i32)
        || key_down(VK_SHIFT.0 as i32);
    {
        let mut st = state().lock().unwrap();
        st.ctrl_pending = ctrl;
    }
    if ctrl {
        return; // Ctrl+click is reveal-on-release, never a drag
    }
    let idx = hit_test(y);
    let Some(path) = entry_path(idx) else {
        return;
    };
    let id = entry_id(idx);
    crate::debuglog::log(&format!("native_thumb: drag start id={id} path={path}"));
    start_drag_job(id, path);
}

unsafe fn on_double_click(hwnd: HWND, lparam: LPARAM) {
    let y = (lparam.0 >> 16) as u16 as i32;
    {
        let mut st = state().lock().unwrap();
        st.last_dbl_ms = now_ms();
        if st.expanded {
            st.expanded = false;
        }
    }
    let (w, h) = current_window_size();
    show_at(hwnd, (w, h), None);
    let _ = InvalidateRect(Some(hwnd), None, false);
    let Some(path) = entry_path(hit_test(y)) else {
        return;
    };
    crate::debuglog::log(&format!("native_thumb: open path={path}"));
    post(Cmd::Open(path));
}

unsafe fn on_button_up(lparam: LPARAM) {
    let y = (lparam.0 >> 16) as u16 as i32;
    let ctrl = {
        let mut st = state().lock().unwrap();
        let c = st.ctrl_pending;
        st.ctrl_pending = false;
        c
    };
    if !ctrl {
        return;
    }
    let Some(path) = entry_path(hit_test(y)) else {
        return;
    };
    crate::debuglog::log(&format!("native_thumb: reveal path={path}"));
    post(Cmd::Reveal(path));
}

fn key_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16) & 0x8000 != 0 }
}

/// Run the OLE drag on the Tauri main thread (same thread as the webview path
/// uses — Chromium-based drop targets reject drags from other threads) and
/// feed the outcome back to the window thread.
fn start_drag_job(capture_id: u64, path: String) {
    let Some(app) = APP.get() else { return };
    let app2 = app.clone();
    let sent = app.run_on_main_thread(move || {
        let outcome = crate::commands::start_drag(app2.clone(), path.clone());
        let (dropped, moved) = outcome
            .map(|o| (o.dropped, o.moved))
            .unwrap_or((false, false));
        crate::debuglog::log(&format!(
            "native_thumb: drag done path={path} dropped={dropped} moved={moved}"
        ));
        let hide_after_drop = crate::settings::get(&app2).hide_after_drop;
        let raw = Box::into_raw(Box::new(Cmd::DragOutcome {
            capture_id,
            dropped,
            moved,
            hide_after_drop,
        }));
        if let Some(&h) = HWND_SLOT.get() {
            unsafe {
                let _ = PostMessageW(Some(hwnd_from(h)), WM_APP_CMD, WPARAM(0), LPARAM(raw as isize));
            }
        }
    });
    if sent.is_err() {
        crate::debuglog::log("native_thumb: drag job not sent (event loop gone)");
    }
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

unsafe fn paint(hwnd: HWND) {
    let mut ps = Default::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    if hdc.is_invalid() {
        return;
    }
    let (w, h) = current_window_size();
    if w > 0 && h > 0 {
        let mem = CreateCompatibleDC(Some(hdc));
        if !mem.is_invalid() {
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bi = bitmap_header(w as u32, h as u32);
            if let Ok(bmp) = CreateDIBSection(Some(hdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
                let old = SelectObject(mem, bmp.into());
                draw_scene(mem, w, h);
                let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
                SelectObject(mem, old);
                let _ = DeleteObject(bmp.into());
            }
            let _ = DeleteDC(mem);
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

fn bitmap_header(w: u32, h: u32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

unsafe fn draw_scene(dc: HDC, w: i32, h: i32) {
    // Background (visible only as slivers around the peeking cards).
    fill(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, PEEK_BG);

    let (cw, ch, expanded, len) = {
        let st = state().lock().unwrap();
        (st.card_w, st.card_h, st.expanded, st.stack.len())
    };

    // Peeking older captures: stack[1] at 95%/9px, stack[2] at 90%/18px.
    let peek_specs = [(1usize, 0.95f64, 9i32), (2, 0.90, 18)];
    for (idx, scale, off) in peek_specs {
        if idx >= len {
            continue;
        }
        let pw = (cw as f64 * scale).round() as i32;
        let ph = (ch as f64 * scale).round() as i32;
        let x = (cw - pw) / 2;
        let y = (ch - ph) / 2 + off;
        let rect = RECT { left: x, top: y, right: x + pw, bottom: y + ph };
        fill(dc, &rect, PEEK_BG);
        draw_entry_image(dc, idx, x, y, pw, ph);
        frame(dc, &rect, BORDER);
    }

    // Current card.
    let card = RECT { left: 0, top: 0, right: cw, bottom: ch };
    fill(dc, &card, CARD_BG);
    if !draw_entry_image(dc, 0, 0, 0, cw, ch) {
        fill(dc, &RECT { left: 8, top: CAPTION_H, right: cw - 8, bottom: ch - 8 }, PLACEHOLDER_BG);
    }
    // Caption bar + filename.
    fill(dc, &RECT { left: 0, top: 0, right: cw, bottom: CAPTION_H }, CAPTION_BG);
    let name = {
        let st = state().lock().unwrap();
        st.stack.first().map(|e| e.file_name.clone()).unwrap_or_default()
    };
    draw_text(dc, &name, RECT { left: 8, top: 0, right: cw - 8, bottom: CAPTION_H }, TEXT);
    // "Not saved" badge.
    let unsaved = {
        let st = state().lock().unwrap();
        st.stack.first().map(|e| e.unsaved).unwrap_or(false)
    };
    if unsaved {
        let badge = RECT { left: 8, top: ch - CAPTION_H - 6, right: 96, bottom: ch - 6 };
        fill(dc, &badge, BADGE_BG);
        draw_text(dc, "Not saved", RECT { left: 14, top: ch - CAPTION_H - 6, right: 90, bottom: ch - 6 }, BADGE_TEXT);
    }
    frame(dc, &card, BORDER);

    // Expanded list rows.
    if expanded && len > 1 {
        for i in 1..len {
            let y0 = ch + (i as i32 - 1) * ROW_H;
            let row = RECT { left: 0, top: y0, right: cw, bottom: y0 + ROW_H };
            fill(dc, &row, ROW_BG);
            draw_entry_image(dc, i, 8, y0 + 4, ROW_H - 8, ROW_H - 8);
            let (name, unsaved) = {
                let st = state().lock().unwrap();
                match st.stack.get(i) {
                    Some(e) => (e.file_name.clone(), e.unsaved),
                    None => (String::new(), false),
                }
            };
            let label = if unsaved { format!("{name} — not saved") } else { name };
            draw_text(dc, &label, RECT { left: ROW_H + 12, top: y0, right: cw - 8, bottom: y0 + ROW_H }, TEXT);
        }
    }
}

/// Draw one stack entry's image into (x, y, w, h). Returns false if the entry
/// has no preview (caller should paint a placeholder).
unsafe fn draw_entry_image(dc: HDC, idx: usize, x: i32, y: i32, w: i32, h: i32) -> bool {
    if w <= 0 || h <= 0 {
        return false;
    }
    let bits = {
        let st = state().lock().unwrap();
        st.stack.get(idx).and_then(|e| e.bits.clone())
    };
    let Some((data, bw, bh)) = bits else {
        return false;
    };
    if data.len() < (bw as usize) * (bh as usize) * 4 {
        return false;
    }
    let _ = SetStretchBltMode(dc, HALFTONE);
    let bi = bitmap_header(bw, bh);
    let drawn = StretchDIBits(
        dc,
        x,
        y,
        w,
        h,
        0,
        0,
        bw as i32,
        bh as i32,
        Some(data.as_ptr() as *const c_void),
        &bi,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
    drawn > 0 /* 0 / GDI_ERROR mean nothing was drawn */
}

unsafe fn fill(dc: HDC, rect: &RECT, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    FillRect(dc, rect, brush);
    let _ = DeleteObject(brush.into());
}

unsafe fn frame(dc: HDC, rect: &RECT, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    FrameRect(dc, rect, brush);
    let _ = DeleteObject(brush.into());
}

unsafe fn draw_text(dc: HDC, text: &str, rect: RECT, color: COLORREF) {
    static FONT: OnceLock<isize> = OnceLock::new();
    let font = *FONT.get_or_init(|| unsafe {
        CreateFontW(
            -15,
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
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        )
        .0 as isize
    });
    if font == 0 {
        return;
    }
    let old = SelectObject(dc, HFONT(font as *mut c_void).into());
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, color);
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    let mut r = rect;
    DrawTextW(dc, &mut buf, &mut r, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_LEFT);
    SelectObject(dc, old);
}

// ---------------------------------------------------------------------------
// Entry construction / policy (pure, testable)
// ---------------------------------------------------------------------------

fn make_entry(e: PresentEntry) -> Entry {
    let file_name = e
        .path
        .as_ref()
        .map(|p| p.rsplit(['\\', '/']).next().unwrap_or(p).to_string())
        .unwrap_or_else(|| "unsaved".to_string());
    Entry {
        capture_id: e.capture_id,
        bits: decode_png(&e.png),
        path: e.path,
        file_name,
        unsaved: e.unsaved,
    }
}

/// Decode the preview PNG into top-down BGRA for GDI.
fn decode_png(png: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(png).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut bgra = Vec::with_capacity(rgba.len());
    for px in rgba.pixels() {
        bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    Some((bgra, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64) -> Entry {
        Entry {
            capture_id: id,
            path: Some(format!("C:\\x\\snap_{id}.png")),
            file_name: format!("snap_{id}.png"),
            bits: Some((vec![0u8; 4], 1, 1)),
            unsaved: false,
        }
    }

    #[test]
    fn stack_dedupes_and_caps_at_ten() {
        let mut stack = Vec::new();
        let mut seen = HashSet::new();
        // First capture lands.
        assert!(stack_push(&mut stack, &mut seen, entry(1)));
        assert_eq!(stack[0].capture_id, 1);
        // A duplicate id is skipped (webview parity: addCapture SKIP).
        assert!(!stack_push(&mut stack, &mut seen, entry(1)));
        assert_eq!(stack.len(), 1);
        // Newest on top.
        assert!(stack_push(&mut stack, &mut seen, entry(2)));
        assert_eq!(stack[0].capture_id, 2);
        // Invalid ids rejected.
        assert!(!stack_push(&mut stack, &mut seen, entry(0)));
        // Cap at 10.
        for id in 3..=15 {
            stack_push(&mut stack, &mut seen, entry(id));
        }
        assert_eq!(stack.len(), 10);
        assert_eq!(stack[0].capture_id, 15);
    }

    #[test]
    fn stack_remove_updates_seen() {
        let mut stack = vec![entry(2), entry(1)];
        let mut seen = HashSet::from([1, 2]);
        assert!(stack_remove(&mut stack, &mut seen, 2));
        assert_eq!(stack.len(), 1);
        assert!(!seen.contains(&2));
        // Removing an unknown id is a no-op.
        assert!(!stack_remove(&mut stack, &mut seen, 99));
    }

    #[test]
    fn decode_png_rejects_garbage_and_decodes_valid() {
        use image::ImageEncoder;
        assert!(decode_png(b"not a png").is_none());
        // Encode a real 2x1 PNG with the same encoder the app uses.
        let img = image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 128, 0, 255]));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 2, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let (data, w, h) = decode_png(&png).expect("valid png decodes");
        assert_eq!((w, h), (2, 1));
        assert_eq!(data.len(), 2 * 1 * 4);
    }
}
