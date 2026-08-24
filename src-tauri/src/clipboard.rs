//! Windows clipboard: CF_DIBV5 (image paste) + CF_HDROP (file paste) in one session.

use std::path::Path;

use windows::core::BOOL;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, POINT};
use windows::Win32::Graphics::Gdi::{BI_COMPRESSION, BITMAPV5HEADER};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
};
use windows::Win32::UI::Shell::DROPFILES;

// LCS_WINDOWS_COLOR_SPACE = 2, BI_BITFIELDS = 3
const LCS_WINDOWS_COLOR_SPACE: u32 = 2;
const BI_BITFIELDS: u32 = 3;
const CF_DIBV5: u32 = 17;
const CF_HDROP: u32 = 15;

/// Put both a DIB image and the file itself on the clipboard.
pub fn set_image_and_file(bgra: &[u8], width: u32, height: u32, path: &Path) -> Result<(), String> {
    unsafe {
        if !OpenClipboard(None).is_ok() {
            return Err("Could not open clipboard".into());
        }
        let result = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| e.to_string())?;

            if let Some(h) = build_dibv5(bgra, width, height) {
                let r = SetClipboardData(CF_DIBV5, Some(HANDLE(h.0)));
                if r.is_err() {
                    let _ = GlobalFree(Some(h));
                }
            }
            if let Some(h) = build_hdrop(path) {
                let r = SetClipboardData(CF_HDROP, Some(HANDLE(h.0)));
                if r.is_err() {
                    let _ = GlobalFree(Some(h));
                }
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

/// Copy only the file (CF_HDROP) — used by the "Copy" thumbnail action.
pub fn set_file(path: &Path) -> Result<(), String> {
    unsafe {
        if !OpenClipboard(None).is_ok() {
            return Err("Could not open clipboard".into());
        }
        let result = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| e.to_string())?;
            if let Some(h) = build_hdrop(path) {
                let r = SetClipboardData(CF_HDROP, Some(HANDLE(h.0)));
                if r.is_err() {
                    let _ = GlobalFree(Some(h));
                }
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

fn build_dibv5(bgra: &[u8], width: u32, height: u32) -> Option<HGLOBAL> {
    unsafe {
        let header = BITMAPV5HEADER {
            bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
            bV5Width: width as i32,
            bV5Height: -(height as i32),
            bV5Planes: 1,
            bV5BitCount: 32,
            bV5Compression: BI_COMPRESSION(BI_BITFIELDS),
            bV5SizeImage: width * height * 4,
            bV5XPelsPerMeter: 0,
            bV5YPelsPerMeter: 0,
            bV5ClrUsed: 0,
            bV5ClrImportant: 0,
            bV5RedMask: 0x00FF_0000,
            bV5GreenMask: 0x0000_FF00,
            bV5BlueMask: 0x0000_00FF,
            bV5AlphaMask: 0xFF00_0000,
            bV5CSType: LCS_WINDOWS_COLOR_SPACE,
            bV5Endpoints: Default::default(),
            bV5GammaRed: 0,
            bV5GammaGreen: 0,
            bV5GammaBlue: 0,
            bV5Intent: 0,
            bV5ProfileData: 0,
            bV5ProfileSize: 0,
            bV5Reserved: 0,
        };
        let header_bytes = std::slice::from_raw_parts(
            (&header as *const BITMAPV5HEADER) as *const u8,
            std::mem::size_of::<BITMAPV5HEADER>(),
        );
        let total = std::mem::size_of::<BITMAPV5HEADER>() + bgra.len();
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total).ok()?;
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            let _ = GlobalFree(Some(h));
            return None;
        }
        std::ptr::copy_nonoverlapping(header_bytes.as_ptr(), ptr as *mut u8, header_bytes.len());
        let px = std::slice::from_raw_parts_mut(
            ptr.add(std::mem::size_of::<BITMAPV5HEADER>()) as *mut u8,
            bgra.len(),
        );
        // BGRA with alpha forced opaque.
        for (i, pxb) in bgra.chunks_exact(4).enumerate() {
            px[i * 4] = pxb[0];
            px[i * 4 + 1] = pxb[1];
            px[i * 4 + 2] = pxb[2];
            px[i * 4 + 3] = 255;
        }
        let _ = GlobalUnlock(h);
        Some(h)
    }
}

fn build_hdrop(path: &Path) -> Option<HGLOBAL> {
    unsafe {
        let mut wide: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
        wide.push(0);
        wide.push(0);
        let df = DROPFILES {
            pFiles: std::mem::size_of::<DROPFILES>() as u32,
            pt: POINT::default(),
            fNC: BOOL(0),
            fWide: BOOL(1),
        };
        let total = std::mem::size_of::<DROPFILES>() + wide.len() * 2;
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total).ok()?;
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            let _ = GlobalFree(Some(h));
            return None;
        }
        std::ptr::copy_nonoverlapping(
            (&df as *const DROPFILES) as *const u8,
            ptr as *mut u8,
            std::mem::size_of::<DROPFILES>(),
        );
        let list = ptr.add(std::mem::size_of::<DROPFILES>()) as *mut u16;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), list, wide.len());

        let _ = GlobalUnlock(h);
        Some(h)
    }
}
