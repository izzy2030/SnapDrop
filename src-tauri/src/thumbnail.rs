//! Floating thumbnail window: sizing, positioning, and capture events.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Size};

use crate::{monitors, settings};

#[derive(Serialize, Clone)]
pub struct CapturedPayload {
    /// Monotonic identity used by the thumbnail renderer to deduplicate event
    /// replays and reconcile captures missed while the display was asleep.
    pub capture_id: u64,
    /// None when the screenshot could not be saved (drag disabled, "not saved" state).
    pub path: Option<String>,
    /// data:image/png;base64,...
    pub preview: String,
    pub width: u32,
    pub height: u32,
    pub unsaved: bool,
}

static LATEST_CAPTURE: Mutex<Option<CapturedPayload>> = Mutex::new(None);
static LAST_CAPTURE_CENTER: Mutex<Option<(i32, i32)>> = Mutex::new(None);
static NEXT_CAPTURE_ID: AtomicU64 = AtomicU64::new(1);
/// Whether the latest thumbnail is expected to remain visible. This is kept
/// separately from the native window state because Windows can lose that state
/// while a display or WebView is being resumed.
static THUMBNAIL_WANTED_VISIBLE: AtomicBool = AtomicBool::new(false);
/// Invalidates delayed presentation retries when a capture starts or the user
/// explicitly hides the thumbnail.
static PRESENTATION_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Milliseconds of the last time the thumbnail renderer polled the backend
/// (`get_latest_capture`). The renderer polls every ~1.5s while alive, so a
/// stale timestamp means the WebView is frozen or wedged (e.g. after display
/// sleep) and the thumbnail is a non-interactive ghost.
static LAST_RENDERER_HEARTBEAT: AtomicU64 = AtomicU64::new(0);
/// Milliseconds of the last presentation attempt (successful or retried).
static LAST_PRESENT_AT: AtomicU64 = AtomicU64::new(0);
/// Milliseconds of the last forced renderer recovery. Throttles the watchdog
/// so a wedged WebView cannot trigger a reload loop.
static LAST_RECOVERY_AT: AtomicU64 = AtomicU64::new(0);

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn get_latest_capture() -> Option<CapturedPayload> {
    // Doubles as the renderer heartbeat: the thumbnail page calls this on a
    // 1.5s poll, so a live page keeps this fresh and the watchdog knows a
    // stale value means the WebView is stuck.
    LAST_RENDERER_HEARTBEAT.store(now_millis(), Ordering::SeqCst);
    LATEST_CAPTURE.lock().unwrap().clone()
}

pub fn hide_all(app: &AppHandle) -> Result<(), String> {
    THUMBNAIL_WANTED_VISIBLE.store(false, Ordering::SeqCst);
    PRESENTATION_GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Some(w) = app.get_webview_window("thumbnail") {
        w.hide().map_err(|e| {
            log::warn!("thumbnail: hide failed: {e}");
            e.to_string()
        })?;
    }
    Ok(())
}

pub fn is_visible(app: &AppHandle) -> bool {
    app.get_webview_window("thumbnail")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

pub fn set_visible(app: &AppHandle, visible: bool) -> Result<(), String> {
    THUMBNAIL_WANTED_VISIBLE.store(visible, Ordering::SeqCst);
    if let Some(w) = app.get_webview_window("thumbnail") {
        let result = if visible { w.show() } else { w.hide() };
        result.map_err(|e| {
            log::warn!("thumbnail: set visible={visible} failed: {e}");
            e.to_string()
        })?;
    }
    Ok(())
}

const MAX_PRESENT_RETRIES: u8 = 6;

/// Show the thumbnail for a freshly captured screenshot.
/// `sel_center` is the selection center in virtual-screen coords (used to pick the monitor).
pub fn show_capture(
    app: &AppHandle,
    path: Option<String>,
    preview_b64: String,
    img_w: u32,
    img_h: u32,
    unsaved: bool,
    sel_center: (i32, i32),
) {
    let settings = settings::get(app);
    if !settings.show_thumbnail {
        return;
    }

    // Store the payload before touching monitor state. Display enumeration can
    // briefly fail while Windows is bringing a sleeping monitor back, but the
    // capture itself has already been saved successfully by this point.
    let payload = CapturedPayload {
        capture_id: NEXT_CAPTURE_ID.fetch_add(1, Ordering::SeqCst),
        path,
        preview: format!("data:image/png;base64,{preview_b64}"),
        width: img_w,
        height: img_h,
        unsaved,
    };
    crate::debuglog::log(&format!(
        "thumbnail: show_capture id={} path={:?} unsaved={} show_thumbnail_setting={} center=({},{})",
        payload.capture_id,
        payload.path,
        payload.unsaved,
        settings.show_thumbnail,
        sel_center.0,
        sel_center.1
    ));
    *LATEST_CAPTURE.lock().unwrap() = Some(payload.clone());
    *LAST_CAPTURE_CENTER.lock().unwrap() = Some(sel_center);
    THUMBNAIL_WANTED_VISIBLE.store(true, Ordering::SeqCst);
    let generation = PRESENTATION_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;

    present_capture(app, payload, img_w, img_h, sel_center, generation, 0);
}

/// Apply the native window state and publish the payload. A failed native
/// operation is retried on Tauri's main thread because WebView/DWM operations
/// can transiently fail immediately after display wake or topology changes.
fn present_capture(
    app: &AppHandle,
    payload: CapturedPayload,
    img_w: u32,
    img_h: u32,
    sel_center: (i32, i32),
    generation: u64,
    attempt: u8,
) {
    if PRESENTATION_GENERATION.load(Ordering::SeqCst) != generation {
        return;
    }
    LAST_PRESENT_AT.store(now_millis(), Ordering::SeqCst);
    let settings = settings::get(app);
    if !settings.show_thumbnail {
        return;
    }

    let (size, position) = thumbnail_geometry(&settings, img_w, img_h, sel_center);
    let mut should_retry = false;

    let mut op_results = Vec::new();
    if let Some(w) = app.get_webview_window("thumbnail") {
        if let Err(e) = w.set_size(Size::Physical(size)) {
            log::warn!("thumbnail: set size failed (attempt {attempt}): {e}");
            op_results.push("size:err".to_string());
            should_retry = true;
        }
        if let Some(position) = position {
            if let Err(e) = w.set_position(position) {
                log::warn!("thumbnail: set position failed (attempt {attempt}): {e}");
                op_results.push("pos:err".to_string());
                should_retry = true;
            }
        }
        if let Err(e) = w.show() {
            log::warn!("thumbnail: show failed (attempt {attempt}): {e}");
            op_results.push("show:err".to_string());
            should_retry = true;
        }
        if !w.is_visible().unwrap_or(false) {
            log::warn!("thumbnail: window still hidden after show (attempt {attempt})");
            op_results.push("visible:false".to_string());
            should_retry = true;
        }
        if let Err(e) = w.unminimize() {
            log::warn!("thumbnail: unminimize failed (attempt {attempt}): {e}");
            op_results.push("unmin:err".to_string());
            should_retry = true;
        }
        if let Err(e) = w.set_always_on_top(true) {
            log::warn!("thumbnail: always-on-top failed (attempt {attempt}): {e}");
            op_results.push("aot:err".to_string());
            should_retry = true;
        }
        op_results.push("ok".to_string());
        log::info!(
            "thumbnail::show_capture pos={:?} size={}x{} path={:?} attempt={attempt}",
            position,
            size.width,
            size.height,
            payload.path
        );
    } else {
        log::error!("thumbnail::show_capture: thumbnail window not found (attempt {attempt})");
        op_results.push("window:missing".to_string());
        should_retry = true;
    }

    // Keep the event for the Settings window's history refresh and for the
    // thumbnail renderer. The renderer also reconciles from LATEST_CAPTURE,
    // so a wake-time event delivery gap is harmless.
    let emit_result = app.emit("thumbnail-captured", payload.clone());
    let emit_ok = emit_result.is_ok();
    if let Err(e) = emit_result {
        log::warn!("thumbnail: captured event failed (attempt {attempt}): {e}");
        should_retry = true;
    }
    crate::debuglog::log(&format!(
        "thumbnail: present id={} attempt={} ops={} emit={} retry={}",
        payload.capture_id,
        attempt,
        op_results.join(","),
        if emit_ok { "ok" } else { "err" },
        should_retry
    ));

    if should_retry && attempt < MAX_PRESENT_RETRIES {
        schedule_present_retry(
            app,
            payload,
            img_w,
            img_h,
            sel_center,
            generation,
            attempt + 1,
        );
    }
}

/// Recalculate the monitor geometry on every retry. If no monitor is available
/// momentarily, still show the window at a virtual-screen fallback position;
/// a missing position must never suppress the thumbnail entirely.
fn thumbnail_geometry(
    settings: &settings::Settings,
    img_w: u32,
    img_h: u32,
    sel_center: (i32, i32),
) -> (PhysicalSize<u32>, Option<PhysicalPosition<i32>>) {
    let base = match settings.thumbnail_size.as_str() {
        "small" => 220,
        "large" => 420,
        _ => 320,
    };
    let aspect = if img_w > 0 && img_h > 0 {
        img_h as f64 / img_w as f64
    } else {
        0.75
    };

    let mon = monitors::monitor_at_point(sel_center.0, sel_center.1)
        .or_else(|| monitors::monitor_at_point(0, 0))
        .or_else(|| monitors::enumerate().into_iter().next());

    if let Some(mon) = mon {
        let scale = mon.scale.max(1.0);
        let margin = (16.0 * scale).round() as i32;
        let work_w = (mon.work.right - mon.work.left - margin.saturating_mul(2)).max(1) as u32;
        let work_h = (mon.work.bottom - mon.work.top - margin.saturating_mul(2)).max(1) as u32;
        let desired_w = (base as f32 * scale).round().max(1.0) as u32;
        let desired_h = (base as f64 * aspect * scale as f64).round().max(1.0) as u32;
        let min_h = (120.0 * scale).round().max(1.0) as u32;
        let win_w = desired_w.min(work_w).max(1);
        let win_h = desired_h.max(min_h.min(work_h)).min(work_h).max(1);
        let (wl, wt, wr, wb) = (mon.work.left, mon.work.top, mon.work.right, mon.work.bottom);
        let left_bound = wl + margin;
        let top_bound = wt + margin;
        let right_bound = (wr - win_w as i32 - margin).max(left_bound);
        let bottom_bound = (wb - win_h as i32 - margin).max(top_bound);
        let (wx, wy) = match settings.thumbnail_position.as_str() {
            "top_left" => (left_bound, top_bound),
            "top_right" => (right_bound, top_bound),
            "bottom_left" => (left_bound, bottom_bound),
            _ => (right_bound, bottom_bound),
        };
        return (
            PhysicalSize::new(win_w, win_h),
            Some(PhysicalPosition::new(wx, wy)),
        );
    }

    let win_w = base;
    let win_h = (base as f64 * aspect).round().max(1.0) as u32;
    let virt = monitors::virtual_screen();
    let position = if virt.right > virt.left && virt.bottom > virt.top {
        Some(PhysicalPosition::new(virt.left + 16, virt.top + 16))
    } else {
        log::warn!("thumbnail: monitor geometry unavailable; showing at current window position");
        None
    };
    (PhysicalSize::new(win_w, win_h), position)
}

fn schedule_present_retry(
    app: &AppHandle,
    payload: CapturedPayload,
    img_w: u32,
    img_h: u32,
    sel_center: (i32, i32),
    generation: u64,
    attempt: u8,
) {
    let app = app.clone();
    std::thread::spawn(move || {
        let delay_ms = 100u64 << (attempt.saturating_sub(1).min(5));
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        let app_for_main = app.clone();
        let result = app.run_on_main_thread(move || {
            present_capture(
                &app_for_main,
                payload,
                img_w,
                img_h,
                sel_center,
                generation,
                attempt,
            );
        });
        if let Err(e) = result {
            log::warn!("thumbnail: could not schedule presentation retry: {e}");
        }
    });
}

/// Recover the thumbnail when the renderer has gone quiet: reload its WebView
/// and re-present the latest capture. Only acts when the renderer's heartbeat
/// (the 1.5s `get_latest_capture` poll) has stopped, so a healthy thumbnail is
/// never touched, and is throttled so a wedged WebView can't cause a reload
/// loop.
const RENDERER_STALE_AFTER_MS: u64 = 10_000;
const RECOVERY_COOLDOWN_MS: u64 = 60_000;

/// Decide whether the thumbnail renderer needs a forced recovery. Pure so the
/// watchdog and resume hook share one throttled policy and it is testable:
/// the renderer heartbeat (`get_latest_capture` poll) stopped, we didn't just
/// present, and we haven't recovered in the cooldown window.
fn should_recover(now: u64, heartbeat: u64, last_present: u64, last_recovery: u64) -> bool {
    if now.saturating_sub(heartbeat) < RENDERER_STALE_AFTER_MS {
        return false; // renderer is alive and polling
    }
    if now.saturating_sub(last_present) < RENDERER_STALE_AFTER_MS {
        return false; // just presented (capture flow mid-presentation)
    }
    if now.saturating_sub(last_recovery) < RECOVERY_COOLDOWN_MS {
        return false; // at most one forced recovery per minute
    }
    true
}

pub fn recover_if_stale(app: &AppHandle) {
    if !THUMBNAIL_WANTED_VISIBLE.load(Ordering::SeqCst) {
        return;
    }
    let now = now_millis();
    let heartbeat = LAST_RENDERER_HEARTBEAT.load(Ordering::SeqCst);
    let last_present = LAST_PRESENT_AT.load(Ordering::SeqCst);
    let last_recovery = LAST_RECOVERY_AT.load(Ordering::SeqCst);
    if !should_recover(now, heartbeat, last_present, last_recovery) {
        return;
    }
    // Read the payload directly (not via `get_latest_capture`) so this recovery
    // does not bump the renderer heartbeat and mask a still-dead WebView.
    let Some(payload) = LATEST_CAPTURE.lock().unwrap().clone() else {
        return;
    };
    let center = LAST_CAPTURE_CENTER.lock().unwrap().unwrap_or((0, 0));
    let generation = PRESENTATION_GENERATION.load(Ordering::SeqCst);
    if generation == 0 {
        return;
    }

    LAST_RECOVERY_AT.store(now, Ordering::SeqCst);
    crate::debuglog::log(&format!(
        "thumbnail: RECOVER stale heartbeat ({}s) -> reload + present id={}",
        (now - heartbeat) / 1000,
        payload.capture_id
    ));
    log::warn!("thumbnail: renderer heartbeat stale ({}s); reloading and re-presenting", (now - heartbeat) / 1000);
    if let Some(w) = app.get_webview_window("thumbnail") {
        if let Err(e) = w.reload() {
            log::warn!("thumbnail: reload after stale heartbeat failed: {e}");
        }
    }
    present_capture(
        app,
        payload.clone(),
        payload.width,
        payload.height,
        center,
        generation,
        0,
    );
}

/// Rebuild the thumbnail WebView's state after the application/display resumes.
/// Re-presentation without a reload is harmless (the renderer dedupes by
/// capture id and reconciles from `LATEST_CAPTURE`); a genuinely frozen
/// renderer is handled by the heartbeat watchdog instead.
pub fn recover_after_resume(app: &AppHandle) {
    recover_if_stale(app);
}

/// Background watchdog: while the thumbnail is meant to be visible, check that
/// the renderer is still polling. A WebView that froze across display sleep
/// stops answering IPC and would otherwise remain a stale, non-draggable ghost
/// until the app restarts. Runs every few seconds on a dedicated thread.
pub fn spawn_renderer_watchdog(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(4));
        // Don't wake the event loop unless the thumbnail is meant to be
        // visible; when hidden (dismissed, auto-hide, or between captures)
        // there is nothing to recover.
        if !THUMBNAIL_WANTED_VISIBLE.load(Ordering::SeqCst) {
            continue;
        }
        let app_for_main = app.clone();
        let sent = app.run_on_main_thread(move || {
            recover_if_stale(&app_for_main);
        });
        if sent.is_err() {
            return; // event loop gone (app shutting down)
        }
    });
}

/// Show the thumbnail for an existing file (tray "Recent Captures").
pub fn show_capture_for(app: &AppHandle, path: &str) {
    show_capture_for_at(app, path, None);
}

/// Show the thumbnail for an existing file, positioned near the given monitor
/// point (`sel_center` in virtual-screen coords).
pub fn show_capture_for_at(app: &AppHandle, path: &str, sel_center: Option<(i32, i32)>) {
    let img = match image::open(path) {
        Ok(i) => i,
        Err(e) => {
            crate::notifier::toast(app, "error", &format!("Could not open {path}: {e}"));
            return;
        }
    };
    use image::GenericImageView;
    let (w, h) = img.dimensions();
    let max_dim = 512u32;
    let (tw, th) = if w.max(h) <= max_dim {
        (w, h)
    } else {
        let s = max_dim as f64 / w.max(h) as f64;
        (
            ((w as f64 * s).round() as u32).max(1),
            ((h as f64 * s).round() as u32).max(1),
        )
    };
    // `thumbnail()` preserves aspect ratio and may return a slightly smaller image
    // than the requested `tw`/`th` (e.g. 511×421 vs 512×421 due to rounding), so
    // use the actual returned dimensions for encoding to avoid a buffer-size panic.
    let thumb = img.thumbnail(tw, th);
    let (actual_tw, actual_th) = thumb.dimensions();
    let thumb = thumb.to_rgba8();
    let mut png = Vec::new();
    use image::ImageEncoder;
    let ok = image::codecs::png::PngEncoder::new(&mut png).write_image(
        thumb.as_raw(),
        actual_tw,
        actual_th,
        image::ExtendedColorType::Rgba8,
    );
    if ok.is_err() {
        crate::notifier::toast(app, "error", "Could not preview capture");
        return;
    }
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    let center = sel_center.unwrap_or(((w / 2) as i32, (h / 2) as i32));
    show_capture(app, Some(path.to_string()), b64, w, h, false, center);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_fires_only_when_renderer_is_stale_and_throttled() {
        let now = 1_000_000u64;
        // Live renderer: heartbeat fresh → never recover.
        assert!(!should_recover(now, now - 1_000, now - 60_000, 0));
        // Stale renderer, but we just presented → wait.
        assert!(!should_recover(now, 0, now - 1_000, 0));
        // Stale renderer, no recent present, but recovered 30s ago → cooldown.
        assert!(!should_recover(now, 0, now - 60_000, now - 30_000));
        // Stale renderer, nothing recent → recover.
        assert!(should_recover(now, 0, now - 60_000, now - 120_000));
        // Recovery just happened → must wait out the cooldown again.
        assert!(!should_recover(now, 0, now - 60_000, now - 1_000));
    }

    #[test]
    fn capture_payload_serializes_identity_for_reconciliation() {
        let payload = CapturedPayload {
            capture_id: 42,
            path: Some(r"C:\\Pictures\\SnapDrop\\capture.png".into()),
            preview: "data:image/png;base64,preview".into(),
            width: 100,
            height: 80,
            unsaved: false,
        };
        let json = serde_json::to_value(payload).unwrap();
        assert_eq!(json.get("capture_id").and_then(|v| v.as_u64()), Some(42));
    }
}
