//! Native Windows file drag-and-drop.
//!
//! Implements IDataObject (CF_HDROP) + IDropSource and runs DoDragDrop /
//! SHDoDragDrop on a dedicated thread with its own message pump, so external
//! applications receive the screenshot exactly as if it were dragged from
//! Windows Explorer.

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::thread;

use serde::Serialize;
use windows::core::{implement, Interface, PCWSTR, HRESULT};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC, HBITMAP,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink,
    IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl, IEnumSTATDATA,
    STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
};
use windows::Win32::System::Ole::{
    CF_HDROP, DoDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, IDropSource, IDropSource_Impl,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON};
use windows::Win32::UI::Shell::{DROPFILES, SHDRAGIMAGE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, RegisterClassExW,
    WINDOW_EX_STYLE, WNDCLASSEXW, WS_POPUP,
};

const DRAGDROP_S_DROP: HRESULT = HRESULT(0x0004_0100);
const DRAGDROP_S_CANCEL: HRESULT = HRESULT(0x0004_0101);
const DRAGDROP_S_USEDEFAULTCURSORS: HRESULT = HRESULT(0x0004_0102);
const S_FALSE_HR: HRESULT = HRESULT(1);
const S_OK_HR: HRESULT = HRESULT(0);
const DV_E_FORMATETC: HRESULT = HRESULT(0x8004_0064u32 as i32);
const OLE_E_ADVISENOTSUPPORTED: HRESULT = HRESULT(0x8004_0003u32 as i32);
const E_UNEXPECTED_HR: HRESULT = HRESULT(0x8000_FFFFu32 as i32);

#[derive(Serialize, Clone)]
pub struct DragOutcome {
    pub dropped: bool,
    pub moved: bool,
}

/// Entry point called from a Tauri command. Tauri commands run synchronously on
/// the main thread, and OLE drag-and-drop must run on the UI thread that owns
/// the app's windows (Chromium-based drop targets — WebView2/Electron, e.g. AI
/// chat clients — reject drags originating from other threads). So the gesture
/// poll (movement threshold) runs on a short-lived worker thread while the main
/// thread waits, then the actual OLE drag runs inline on the main thread.
/// DoDragDrop runs its own modal message loop, so the main thread stays
/// responsive to the OS for the duration of the drag.
pub fn start_drag(_app: &tauri::AppHandle, path: &str) -> Result<DragOutcome, String> {
    if !Path::new(path).exists() {
        return Err("File not found".into());
    }
    let path = path.to_string();
    let (tx, rx) = mpsc::channel();

    // Poll globally until the cursor actually moves (real drag) or the button
    // is released (plain click → no drag). The webview can't track the pointer
    // once it leaves the small thumbnail window, so poll GetAsyncKeyState/
    // GetCursorPos on a worker thread. Main thread waits (the thumbnail is
    // hidden and the app is idle during this sub-second hold).
    thread::spawn(move || {
        let moved = wait_for_drag_movement();
        let _ = tx.send(moved);
    });

    let moved = rx.recv().map_err(|e| e.to_string())?;
    if !moved {
        return Ok(DragOutcome {
            dropped: false,
            moved: false,
        });
    }

    eprintln!("[snapdrop] drag: starting on thread {:?}", std::thread::current().id());
    Ok(unsafe { run_drag_inline(&path) })
}

/// Runs the whole drag on the calling thread — the app's main thread, since
/// Tauri dispatches commands there. COM is initialized apartment-threaded, the
/// drag-source window is created on this thread, and the drag image is built
/// and destroyed here too (GDI objects are thread-affine).
unsafe fn run_drag_inline(path: &str) -> DragOutcome {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let hwnd = create_drag_source_window();

    let data: IDataObject = DropData {
        path: path.to_string(),
    }
    .into();
    let source: IDropSource = DropSource.into();

    let mut effect: DROPEFFECT = DROPEFFECT::default();
    let ok_effects = DROPEFFECT_COPY | DROPEFFECT_MOVE;

    let drag_image = build_drag_image(path);

    let hr = match &drag_image {
        Some((bmp, _hdc, size, offset)) => {
            let sd = SHDRAGIMAGE {
                sizeDragImage: *size,
                ptOffset: *offset,
                hbmpDragImage: *bmp,
                crColorKey: COLORREF(0xFFFF_FFFF), // CLR_NONE → alpha-blend the bitmap
            };
            // windows 0.62's SHDoDragDrop binding dropped the drag-image param, so
            // call the shell32 export directly. Fall back to plain DoDragDrop if
            // the shell rejects the drag-image variant.
            let hr = SHDoDragDrop(
                hwnd,
                data.as_raw(),
                source.as_raw(),
                ok_effects.0,
                &mut effect.0,
                &sd,
            );
            if hr.is_ok() {
                hr
            } else {
                eprintln!("[snapdrop] drag: SHDoDragDrop failed ({hr:?}), falling back to DoDragDrop");
                effect = DROPEFFECT::default();
                DoDragDrop(&data, &source, ok_effects, &mut effect)
            }
        }
        None => DoDragDrop(&data, &source, ok_effects, &mut effect),
    };

    eprintln!("[snapdrop] drag: DoDragDrop finished, hr={hr:?} effect={effect:?}");

    if let Some((bmp, hdc, _, _)) = drag_image {
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(hdc);
    }
    let _ = DestroyWindow(hwnd);
    CoUninitialize();

    let dropped = hr == DRAGDROP_S_DROP;
    let moved = dropped && effect.contains(DROPEFFECT_MOVE);
    DragOutcome { dropped, moved }
}

/// Poll the global mouse state until the left button is released (click — no
/// drag) or the cursor has moved enough to be a real drag. Esc cancels.
fn wait_for_drag_movement() -> bool {
    let Some(start) = cursor_pos() else {
        return false;
    };
    let threshold = 6.0f64;
    loop {
        if key_down(VK_ESCAPE.0 as i32) {
            return false;
        }
        if !key_down(VK_LBUTTON.0 as i32) {
            return false;
        }
        if let Some(cur) = cursor_pos() {
            let dx = (cur.x - start.x) as f64;
            let dy = (cur.y - start.y) as f64;
            if dx * dx + dy * dy >= threshold * threshold {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}

fn key_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16) & 0x8000 != 0 }
}

fn cursor_pos() -> Option<POINT> {
    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt).ok()? };
    Some(pt)
}

#[link(name = "shell32")]
unsafe extern "system" {
    /// SHDoDragDrop with the SHDRAGIMAGE drag-image parameter.
    fn SHDoDragDrop(
        hwnd: HWND,
        pdata: *mut std::ffi::c_void,
        pdsrc: *mut std::ffi::c_void,
        dwokeffects: u32,
        pdweffect: *mut u32,
        pshd: *const SHDRAGIMAGE,
    ) -> HRESULT;
}

fn drag_source_class() -> &'static [u16] {
    static CLASS: OnceLock<&'static [u16]> = OnceLock::new();
    *CLASS.get_or_init(|| {
        let wide: Vec<u16> = "SnapDropDragSource"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        Box::leak(wide.into_boxed_slice())
    })
}

fn create_drag_source_window() -> HWND {
    unsafe {
        let hinst = HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap().0);
        let class = drag_source_class();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(drag_wndproc),
            hInstance: hinst,
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
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
        .unwrap_or(HWND::default())
    }
}

unsafe extern "system" fn drag_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

// ---------------------------------------------------------------------------
// Drag image
// ---------------------------------------------------------------------------

/// Downscale the screenshot and build a 32-bit premultiplied-ARGB DIB for the
/// shell drag image. Returns (bitmap, memdc, size, cursor offset).
fn build_drag_image(path: &str) -> Option<(HBITMAP, HDC, SIZE, POINT)> {
    use image::GenericImageView;
    let img = image::open(path).ok()?;
    let (w, h) = img.dimensions();
    let max_dim = 256u32;
    let (tw, th) = if w.max(h) <= max_dim {
        (w, h)
    } else {
        let s = max_dim as f64 / w.max(h) as f64;
        (
            ((w as f64 * s).round() as u32).max(1),
            ((h as f64 * s).round() as u32).max(1),
        )
    };
    let thumb = img.thumbnail(tw, th);
    let (actual_tw, actual_th) = thumb.dimensions();
    let rgba = thumb.to_rgba8().into_raw();

    unsafe {
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return None;
        }
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: actual_tw as i32,
                biHeight: -(actual_th as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [Default::default()],
        };
        let bmp = CreateDIBSection(Some(hdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let _ = SelectObject(hdc, bmp.into());
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (actual_tw * actual_th) as usize);
        for (i, p) in rgba.chunks_exact(4).enumerate() {
            let (r, g, b, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
            // Premultiplied BGRA.
            px[i] = (a << 24)
                | (((b * a) / 255) << 16)
                | (((g * a) / 255) << 8)
                | ((r * a) / 255);
        }
        let size = SIZE {
            cx: actual_tw as i32,
            cy: actual_th as i32,
        };
        let offset = POINT {
            x: -(actual_tw as i32 / 2),
            y: -(actual_th as i32 / 2),
        };
        Some((bmp, hdc, size, offset))
    }
}

// ---------------------------------------------------------------------------
// IDataObject
// ---------------------------------------------------------------------------

#[implement(IDataObject)]
struct DropData {
    path: String,
}

fn build_hdrop_global(path: &str) -> windows::core::Result<HGLOBAL> {
    unsafe {
        let mut wide: Vec<u16> = path.encode_utf16().collect();
        wide.push(0);
        wide.push(0);
        let df = DROPFILES {
            pFiles: size_of::<DROPFILES>() as u32,
            pt: POINT::default(),
            fNC: windows::core::BOOL(0),
            fWide: windows::core::BOOL(1),
        };
        let total = size_of::<DROPFILES>() + wide.len() * 2;
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total)?;
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            let _ = windows::Win32::Foundation::GlobalFree(Some(h));
            return Err(windows::core::Error::from_hresult(HRESULT(87)));
        }
        std::ptr::copy_nonoverlapping(
            (&df as *const DROPFILES) as *const u8,
            ptr as *mut u8,
            size_of::<DROPFILES>(),
        );
        let list = ptr.add(size_of::<DROPFILES>()) as *mut u16;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), list, wide.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

impl IDataObject_Impl for DropData_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        unsafe {
            let fmt = &*pformatetcin;
            if fmt.cfFormat == CF_HDROP.0 && fmt.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
                let h = build_hdrop_global(&self.this.path)?;
                Ok(STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: STGMEDIUM_0 { hGlobal: h },
                    pUnkForRelease: std::mem::ManuallyDrop::new(None),
                })
            } else {
                Err(DV_E_FORMATETC.into())
            }
        }
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(E_UNEXPECTED_HR.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        unsafe {
            let fmt = &*pformatetc;
            if fmt.cfFormat == CF_HDROP.0
                && fmt.tymed & TYMED_HGLOBAL.0 as u32 != 0
                && fmt.dwAspect == DVASPECT_CONTENT.0
            {
                S_OK_HR
            } else {
                DV_E_FORMATETC
            }
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        unsafe {
            *pformatetcout = *pformatectin;
        }
        S_OK_HR
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: windows::core::BOOL,
    ) -> windows::core::Result<()> {
        Err(E_UNEXPECTED_HR.into())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection == DATADIR_GET.0 as u32 {
            let e = FormatEnum::new(FORMATETC {
                cfFormat: CF_HDROP.0,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            });
            Ok(e.into())
        } else {
            Err(E_UNEXPECTED_HR.into())
        }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: windows::core::Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

// ---------------------------------------------------------------------------
// IEnumFORMATETC
// ---------------------------------------------------------------------------

#[implement(IEnumFORMATETC)]
struct FormatEnum {
    fmt: FORMATETC,
    pos: Cell<u32>,
}

impl FormatEnum {
    fn new(fmt: FORMATETC) -> Self {
        Self {
            fmt,
            pos: Cell::new(0),
        }
    }
}

impl IEnumFORMATETC_Impl for FormatEnum_Impl {
    fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
        unsafe {
            if self.this.pos.get() == 0 && celt >= 1 {
                *rgelt = self.this.fmt;
                self.this.pos.set(1);
                if !pceltfetched.is_null() {
                    *pceltfetched = 1;
                }
                S_OK_HR
            } else {
                if !pceltfetched.is_null() {
                    *pceltfetched = 0;
                }
                S_FALSE_HR
            }
        }
    }

    fn Skip(&self, _celt: u32) -> windows::core::Result<()> {
        Err(S_FALSE_HR.into())
    }

    fn Reset(&self) -> windows::core::Result<()> {
        self.this.pos.set(0);
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
        Ok(FormatEnum {
            fmt: self.this.fmt,
            pos: Cell::new(0),
        }
        .into())
    }
}

// ---------------------------------------------------------------------------
// IDropSource
// ---------------------------------------------------------------------------

#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(
        &self,
        fescapepressed: windows::core::BOOL,
        grfkeystate: MODIFIERKEYS_FLAGS,
    ) -> HRESULT {
        if fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if !grfkeystate.contains(MK_LBUTTON) {
            return DRAGDROP_S_DROP;
        }
        S_OK_HR
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
