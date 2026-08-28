//! Offline OCR via Windows.Media.Ocr (WinRT). 100% local — no cloud, no keys.
//!
//! `recognize` takes the capture's tightly-packed BGRA buffer and returns the
//! recognized text. It blocks the calling thread for the recognition (~100ms
//! for a typical region), so callers run it on a worker thread, never on the
//! UI thread.

use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

/// Recognize text in a tightly-packed BGRA buffer (w*h*4 bytes).
pub fn recognize(bgra: &[u8], width: u32, height: u32) -> Result<String, String> {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    unsafe {
        // WinRT needs an apartment on this thread. Workers are plain threads;
        // init as MTA and ignore S_FALSE / RPC_E_CHANGED_MODE (already init'd).
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    if width == 0 || height == 0 || bgra.len() < (width as usize) * (height as usize) * 4 {
        return Err("Empty capture region".into());
    }

    let engine = create_engine().ok_or_else(|| {
        "No OCR language installed. Add one in Windows Settings > Time & Language > Language & region."
            .to_string()
    })?;

    // SoftwareBitmap::CreateCopyFromBuffer needs an IBuffer with exactly
    // w*h*4 bytes; DataWriter is the standard way to build one from a slice.
    let writer = DataWriter::new().map_err(|e| format!("OCR buffer: {e}"))?;
    writer.WriteBytes(bgra).map_err(|e| format!("OCR buffer: {e}"))?;
    let buffer = writer.DetachBuffer().map_err(|e| format!("OCR buffer: {e}"))?;

    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        width as i32,
        height as i32,
    )
    .map_err(|e| format!("OCR bitmap: {e}"))?;

    let result = engine
        .RecognizeAsync(&bitmap)
        .map_err(|e| format!("OCR engine: {e}"))?
        .join()
        .map_err(|e| format!("OCR recognition failed: {e}"))?;

    let text = result.Text().map_err(|e| format!("OCR text: {e}"))?;
    Ok(normalize(&text.to_string_lossy()))
}

/// User profile languages first, then any installed recognizer language.
fn create_engine() -> Option<OcrEngine> {
    if let Ok(e) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Some(e);
    }
    if let Ok(langs) = OcrEngine::AvailableRecognizerLanguages() {
        let count = langs.Size().unwrap_or(0);
        for i in 0..count {
            if let Ok(lang) = langs.GetAt(i) {
                if let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang) {
                    return Some(e);
                }
            }
        }
    }
    None
}

/// OCR returns CRLF line breaks and occasionally pads lines; tidy up without
/// reflowing the original layout (paragraph breaks are preserved).
fn normalize(s: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in s.lines() {
        out.push(line.trim().to_string());
    }
    while out.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_and_keeps_layout() {
        assert_eq!(normalize("  hello \r\n\r\nworld  \r\n"), "hello\n\nworld");
        assert_eq!(normalize("   \r\n  \r\n"), "");
        assert_eq!(normalize("one"), "one");
    }
}
