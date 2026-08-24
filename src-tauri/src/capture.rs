//! Per-monitor GDI screen capture in physical pixels.

use std::ffi::c_void;
use std::mem::size_of;

use windows::core::PCWSTR;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDCW, DeleteDC, DeleteObject,
    GetDIBits, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    SRCCOPY,
};

use crate::monitors::{self, MonitorInfo};

pub struct ImageBuf {
    pub width: u32,
    pub height: u32,
    /// BGRA8, top-down, alpha byte always 0x00 (GDI convention).
    pub bgra: Vec<u8>,
}

/// Capture the given virtual-screen rect (physical pixels) by stitching per-monitor buffers.
pub fn capture_region(sel: &RECT, monitors: &[MonitorInfo]) -> Option<ImageBuf> {
    let w = monitors::rect_width(sel);
    let h = monitors::rect_height(sel);
    if w <= 0 || h <= 0 {
        return None;
    }
    let (w, h) = (w as u32, h as u32);
    let mut out = vec![0u8; w as usize * h as usize * 4];

    for mon in monitors {
        let inter = monitors::intersect(sel, &mon.rect);
        if monitors::is_empty(&inter) {
            continue;
        }
        let full = capture_monitor(mon)?;
        let iw = monitors::rect_width(&inter) as usize;
        let ih = monitors::rect_height(&inter) as usize;
        let src_x = (inter.left - mon.rect.left) as usize;
        let src_y = (inter.top - mon.rect.top) as usize;
        let dst_x = (inter.left - sel.left) as usize;
        let dst_y = (inter.top - sel.top) as usize;

        for row in 0..ih {
            let src_start = (src_y + row) * full.width as usize * 4 + src_x * 4;
            let dst_start = (dst_y + row) * w as usize * 4 + dst_x * 4;
            out[dst_start..dst_start + iw * 4]
                .copy_from_slice(&full.bgra[src_start..src_start + iw * 4]);
        }
    }
    Some(ImageBuf {
        width: w,
        height: h,
        bgra: out,
    })
}

/// Capture one full monitor into a BGRA buffer at its physical resolution.
pub fn capture_monitor(mon: &MonitorInfo) -> Option<ImageBuf> {
    let w = monitors::rect_width(&mon.rect);
    let h = monitors::rect_height(&mon.rect);
    if w <= 0 || h <= 0 {
        return None;
    }

    unsafe {
        let screen = CreateDCW(
            PCWSTR::null(),
            PCWSTR(mon.device.as_ptr()),
            PCWSTR::null(),
            None,
        );
        if screen.is_invalid() {
            return None;
        }
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp.into());
        let ok = BitBlt(mem, 0, 0, w, h, Some(screen), 0, 0, SRCCOPY).is_ok();

        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [Default::default()],
        };
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bi,
            DIB_RGB_COLORS,
        );

        let _ = SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = DeleteDC(screen);

        if !ok || lines == 0 {
            return None;
        }
        Some(ImageBuf {
            width: w as u32,
            height: h as u32,
            bgra: buf,
        })
    }
}

#[allow(dead_code)]
fn _hgd(obj: HGDIOBJ) {
    let _ = obj;
}
