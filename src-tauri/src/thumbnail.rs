//! Floating thumbnail window: sizing, positioning, and capture events.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
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
/// Milliseconds of the last input event (pointer/keyboard/wheel) the thumbnail
/// renderer received. The renderer reports these (throttled) via
/// `report_renderer_input`; a fresh heartbeat with a stale input timestamp
/// while the window is on screen means JS is running but the WebView's input
/// pipeline/compositor is stuck — the "ghost" that looks visible but ignores
/// drags. This is invisible to the heartbeat-only watchdog.
static LAST_RENDERER_INPUT: AtomicU64 = AtomicU64::new(0);
/// Milliseconds of the last `pointerdown` the thumbnail renderer received.
/// Reported immediately (unthrottled) via `report_renderer_pointerdown`.
/// Unlike the generic input timestamp, this one distinguishes "moves reached
/// the page" from "a click was received". A click that never produces a drag
/// is the exact ghost signature: the page saw the press but its React event
/// handling is wedged, so the user can look at it, wiggle the mouse, and drag
/// nothing.
static LAST_RENDERER_POINTERDOWN: AtomicU64 = AtomicU64::new(0);
/// Milliseconds of the last time the renderer successfully started a native
/// drag (`start_drag` command). Compared against `LAST_RENDERER_POINTERDOWN`
/// by the click-stall watchdog: a healthy page starts a drag within moments of
/// the press; a ghost never does.
static LAST_DRAG_STARTED_AT: AtomicU64 = AtomicU64::new(0);
/// Whether the renderer received any input since the last forced recovery.
/// Cleared by every recovery and set by `report_renderer_input`/
/// `report_renderer_pointerdown`. The input-stall rule requires it so an
/// untouched visible thumbnail is recovered at most once per "episode" —
/// without it, a thumbnail the user walked away from would be reloaded every
/// minute forever.
static INPUT_SINCE_RECOVERY: AtomicBool = AtomicBool::new(true);
/// Milliseconds of the last presentation attempt (successful or retried).
static LAST_PRESENT_AT: AtomicU64 = AtomicU64::new(0);
/// Milliseconds of the last forced renderer recovery. Throttles the watchdog
/// so a wedged WebView cannot trigger a reload loop.
static LAST_RECOVERY_AT: AtomicU64 = AtomicU64::new(0);
/// Milliseconds of the last proactive refresh-before-present. Tracked
/// separately from `LAST_RECOVERY_AT`: arming the watchdog cooldown here made
/// a click-stall recovery wait out the full 60s cooldown in the field,
/// leaving the ghost undraggable ~17s longer than the 2.5s policy intends.
static LAST_REFRESH_AT: AtomicU64 = AtomicU64::new(0);
/// Serializes WebView recovery so overlapping watchdog/resume callbacks cannot
/// reload and present the same window concurrently.
static RECOVERY_IN_PROGRESS: OnceLock<Mutex<()>> = OnceLock::new();

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
    let now = now_millis();
    LAST_RENDERER_HEARTBEAT.store(now, Ordering::SeqCst);
    let payload = LATEST_CAPTURE.lock().unwrap().clone();
    crate::debuglog::log(&format!(
        "thumbnail: IPC get_latest_capture heartbeat={} payload_id={} preview_len={} input_age_s={} click_age_s={}",
        now,
        payload.as_ref().map(|p| p.capture_id).unwrap_or(0),
        payload.as_ref().map(|p| p.preview.len()).unwrap_or(0),
        now.saturating_sub(LAST_RENDERER_INPUT.load(Ordering::SeqCst)) / 1000,
        now.saturating_sub(LAST_RENDERER_POINTERDOWN.load(Ordering::SeqCst)) / 1000
    ));
    payload
}

/// Record that the thumbnail renderer received a user input event. Called
/// (throttled by the renderer) on pointer/keyboard/wheel activity so the
/// watchdog can distinguish a healthy page from a "ghost" whose JS keeps
/// running but whose input pipeline is stuck.
pub fn report_renderer_input() {
    LAST_RENDERER_INPUT.store(now_millis(), Ordering::SeqCst);
    INPUT_SINCE_RECOVERY.store(true, Ordering::SeqCst);
}

/// Record that the thumbnail renderer received a `pointerdown`. Reported
/// immediately (not throttled) so the watchdog can tell a plain hover from a
/// click that was never turned into a drag.
pub fn report_renderer_pointerdown() {
    LAST_RENDERER_POINTERDOWN.store(now_millis(), Ordering::SeqCst);
    INPUT_SINCE_RECOVERY.store(true, Ordering::SeqCst);
}

/// Record that a native drag was started (`start_drag` command). Feeds the
/// click-stall watchdog: press → drag should follow within moments.
pub fn note_drag_started() {
    LAST_DRAG_STARTED_AT.store(now_millis(), Ordering::SeqCst);
}

pub fn hide_all(app: &AppHandle) -> Result<(), String> {
    THUMBNAIL_WANTED_VISIBLE.store(false, Ordering::SeqCst);
    let generation = PRESENTATION_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    crate::debuglog::log(&format!("thumbnail: hide requested generation={generation}"));
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
    crate::debuglog::log(&format!("thumbnail: set_visible requested visible={visible}"));
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

    // Long-idle ghost prevention: the renderer that sat hidden for minutes (or
    // overnight) is the one whose frame goes stale and whose drags die. If it
    // has received no input for a while, reload it before showing the new
    // capture so the capture lands on a fresh page. Throttled by its own
    // marker so rapid successive captures never reload twice — deliberately
    // NOT by the recovery cooldown: arming it here delayed a click-stall
    // recovery by the full 60s in the field.
    let last_input = LAST_RENDERER_INPUT.load(Ordering::SeqCst);
    let idle_ms = now_millis().saturating_sub(last_input);
    let now_ms = now_millis();
    if should_refresh_before_present(
        now_ms,
        last_input,
        idle_ms,
        LAST_REFRESH_AT.load(Ordering::SeqCst),
        LAST_RECOVERY_AT.load(Ordering::SeqCst),
    ) {
        LAST_REFRESH_AT.store(now_ms, Ordering::SeqCst);
        crate::debuglog::log(&format!(
            "thumbnail: refreshing renderer after {idle_ms}ms idle before presenting id={}",
            payload.capture_id
        ));
        if let Some(w) = app.get_webview_window("thumbnail") {
            if let Err(e) = w.reload() {
                log::warn!("thumbnail: refresh-before-present reload failed: {e}");
            }
        }
    }

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
    let present_started = now_millis();
    LAST_PRESENT_AT.store(present_started, Ordering::SeqCst);
    let settings = settings::get(app);
    if !settings.show_thumbnail {
        return;
    }

    let (size, position) = thumbnail_geometry(&settings, img_w, img_h, sel_center);
    let mut should_retry = false;

    let mut op_results = Vec::new();
    if let Some(w) = app.get_webview_window("thumbnail") {
        crate::debuglog::log(&format!(
            "thumbnail: native state before present id={} attempt={} visible={:?} size={}x{} position={:?}",
            payload.capture_id,
            attempt,
            w.is_visible().ok(),
            size.width,
            size.height,
            position
        ));
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
        "thumbnail: present id={} attempt={} ops={} emit={} retry={} elapsed_ms={} native_visible={:?}",
        payload.capture_id,
        attempt,
        op_results.join(","),
        if emit_ok { "ok" } else { "err" },
        should_retry,
        now_millis().saturating_sub(present_started),
        app.get_webview_window("thumbnail").and_then(|w| w.is_visible().ok())
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
/// How long the thumbnail can sit on screen without a single input event
/// before the watchdog treats it as a "ghost": the renderer's JS is alive
/// (fresh heartbeat) but the WebView's input/compositor pipeline is stuck, so
/// the visible frame is stale and drags do nothing. A user who stares at the
/// thumbnail without touching it is an innocent false positive — the reload
/// re-presents the same capture invisibly.
const INPUT_STALL_MS: u64 = 45_000;
/// How long to wait after a `pointerdown` before declaring the click stalled.
/// A healthy page starts the native drag within moments of the press (the
/// renderer initiates it synchronously in `onPointerDown`), so a click that
/// produced no drag after this grace is a wedged renderer. Kept short (2.5s)
/// so recovery beats the user dismissing the broken thumbnail with Esc
/// (observed: users press Esc ~6s after a failed drag attempt).
const CLICK_STALL_GRACE_MS: u64 = 2_500;
/// A click older than this no longer counts as "the user just tried to drag"
/// (avoids firing long after the fact once the cooldown expires).
const CLICK_STALL_WINDOW_MS: u64 = 30_000;
/// After this much renderer input-idle, a new capture is presented to a fresh
/// page: the long-idle ghost (stale frame + dead drags) forms while the page
/// sits hidden for minutes, so the first capture afterwards gets reloaded
/// before it is shown.
const LONG_IDLE_RELOAD_MS: u64 = 60_000;

/// Decide whether the thumbnail renderer needs a forced recovery because its
/// JS heartbeat (`get_latest_capture` poll) stopped. Pure so the watchdog and
/// resume hook share one throttled policy and it is testable: the heartbeat
/// stopped, we didn't just present, and we haven't recovered in the cooldown.
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

/// Decide whether the thumbnail needs recovery because a `pointerdown` was
/// received but no drag ever started: the page saw the click (so the WebView
/// is alive and IPC works) but its event handling is wedged, leaving a
/// visible-but-undraggable ghost. Pure and testable.
fn should_recover_click_stall(
    now: u64,
    heartbeat: u64,
    last_pointerdown: u64,
    last_drag: u64,
    last_present: u64,
    last_recovery: u64,
) -> bool {
    if now.saturating_sub(heartbeat) >= RENDERER_STALE_AFTER_MS {
        return false; // renderer dead — the heartbeat path handles this
    }
    if last_pointerdown == 0 {
        return false; // no click ever reached the page
    }
    if now.saturating_sub(last_pointerdown) < CLICK_STALL_GRACE_MS {
        return false; // give the healthy page a moment to start the drag
    }
    if now.saturating_sub(last_pointerdown) > CLICK_STALL_WINDOW_MS {
        return false; // stale click; don't recover on ancient history
    }
    if last_drag >= last_pointerdown {
        return false; // a drag started after the press — healthy
    }
    if now.saturating_sub(last_present) < CLICK_STALL_GRACE_MS {
        return false; // just presented; let the user reach for it
    }
    if now.saturating_sub(last_recovery) < RECOVERY_COOLDOWN_MS {
        return false; // at most one forced recovery per minute
    }
    true
}

/// Decide whether to proactively reload the renderer before presenting a
/// capture that arrives after a long idle. Tracked with its own throttle
/// marker (not the recovery cooldown) so it can never delay a watchdog
/// recovery; a page that was just force-recovered is already fresh, so a
/// recent recovery still skips the refresh. Pure and testable.
fn should_refresh_before_present(
    now: u64,
    last_input: u64,
    idle_ms: u64,
    last_refresh: u64,
    last_recovery: u64,
) -> bool {
    if last_input == 0 {
        return false; // brand-new page that never reported input — nothing to refresh
    }
    if idle_ms <= LONG_IDLE_RELOAD_MS {
        return false; // recent input — the page is warm
    }
    if now.saturating_sub(last_refresh) < RECOVERY_COOLDOWN_MS {
        return false; // rapid successive captures: at most one refresh per cooldown
    }
    if now.saturating_sub(last_recovery) < RECOVERY_COOLDOWN_MS {
        return false; // just force-recovered — the page is already fresh
    }
    true
}

/// Decide whether the thumbnail needs recovery because the renderer is *alive*
/// (fresh heartbeat) but has received no input for a long time: the JS keeps
/// running (IPC + timers work, heartbeat stays fresh) while the visible frame
/// and input delivery are stuck after a long idle/sleep. Pure and testable.
/// `input_since_recovery` is required so an untouched visible thumbnail is
/// recovered at most once per episode: if the reload didn't restore input and
/// the user isn't interacting, reloading every minute accomplishes nothing.
fn should_recover_input_stall(
    now: u64,
    heartbeat: u64,
    last_input: u64,
    last_present: u64,
    last_recovery: u64,
    input_since_recovery: bool,
) -> bool {
    if now.saturating_sub(heartbeat) >= RENDERER_STALE_AFTER_MS {
        return false; // renderer dead — the heartbeat path handles this
    }
    if !input_since_recovery {
        return false; // no interaction since the last recovery — don't churn
    }
    if now.saturating_sub(last_input) < INPUT_STALL_MS {
        return false; // user recently interacted (or is actively dragging)
    }
    if now.saturating_sub(last_present) < INPUT_STALL_MS {
        return false; // just presented; give the user a moment to interact
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
    let recovery_lock = RECOVERY_IN_PROGRESS.get_or_init(|| Mutex::new(()));
    let _recovery_guard = recovery_lock.lock().unwrap_or_else(|e| e.into_inner());

    // Re-check after waiting: another watchdog tick may have recovered it.
    if !THUMBNAIL_WANTED_VISIBLE.load(Ordering::SeqCst) {
        return;
    }
    let now = now_millis();
    let heartbeat = LAST_RENDERER_HEARTBEAT.load(Ordering::SeqCst);
    let last_input = LAST_RENDERER_INPUT.load(Ordering::SeqCst);
    let last_present = LAST_PRESENT_AT.load(Ordering::SeqCst);
    let last_recovery = LAST_RECOVERY_AT.load(Ordering::SeqCst);

    // Path 1: the renderer's JS heartbeat stopped (dead/frozen WebView).
    if should_recover(now, heartbeat, last_present, last_recovery) {
        return recover_now(app, now, heartbeat, last_input, "stale heartbeat");
    }
    // Path 2: JS is alive but the page stopped receiving input while the
    // window is on screen — a compositor/input ghost after long idle (visible
    // but stale, drags do nothing). Skipped during an active drag (the OLE
    // drag loop owns the pointer, so the webview legitimately sees no events).
    if should_recover_input_stall(
        now,
        heartbeat,
        last_input,
        last_present,
        last_recovery,
        INPUT_SINCE_RECOVERY.load(Ordering::SeqCst),
    ) && is_visible(app)
        && !crate::dragdrop::is_drag_active()
    {
        return recover_now(app, now, heartbeat, last_input, "input-stalled");
    }
    // Path 3: a pointerdown reached the page but no drag ever started — the
    // user pressed the thumbnail and nothing happened. Distinct from path 2
    // (which needs 45s of *no* input): here input (moves) may be flowing fine,
    // it is specifically the click→drag processing that is wedged.
    if should_recover_click_stall(
        now,
        heartbeat,
        LAST_RENDERER_POINTERDOWN.load(Ordering::SeqCst),
        LAST_DRAG_STARTED_AT.load(Ordering::SeqCst),
        last_present,
        last_recovery,
    ) && is_visible(app)
        && !crate::dragdrop::is_drag_active()
    {
        return recover_now(app, now, heartbeat, last_input, "click-stalled");
    }
}

/// Reload the thumbnail WebView and re-present the latest capture. Shared by
/// the heartbeat and input-stall recovery paths; rechecks that the thumbnail
/// is still wanted before acting so a dismissal can't be resurrected.
fn recover_now(app: &AppHandle, now: u64, heartbeat: u64, last_input: u64, reason: &str) {
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
    // A hide/capture can invalidate this recovery while we were waiting for
    // the lock; never resurrect a thumbnail the user dismissed.
    if !THUMBNAIL_WANTED_VISIBLE.load(Ordering::SeqCst) {
        return;
    }

    LAST_RECOVERY_AT.store(now, Ordering::SeqCst);
    // A recovery is a fresh start: restart the input-stall clock and require
    // the user to interact before an input-stall recovery can fire again, so
    // an untouched thumbnail is never reloaded in a loop.
    LAST_RENDERER_INPUT.store(now, Ordering::SeqCst);
    INPUT_SINCE_RECOVERY.store(false, Ordering::SeqCst);
    crate::debuglog::log(&format!(
        "thumbnail: RECOVER {reason} ({}s since heartbeat, {}s since input) -> reload + present id={}",
        (now - heartbeat) / 1000,
        (now - last_input) / 1000,
        payload.capture_id
    ));
    log::warn!("thumbnail: renderer {reason} ({}s); reloading and re-presenting", (now - heartbeat) / 1000);
    if let Some(w) = app.get_webview_window("thumbnail") {
        if let Err(e) = w.reload() {
            log::warn!("thumbnail: reload after {reason} failed: {e}");
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
/// until the app restarts. Runs every second on a dedicated thread — the
/// click-stall rule (2.5s grace) must beat the user dismissing a broken
/// thumbnail with Esc, which they do ~6s after a failed drag, so the tick
/// cannot be coarse.
pub fn spawn_renderer_watchdog(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
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
    fn input_stall_recovery_fires_only_for_live_renderer_with_no_input() {
        let now = 1_000_000u64;
        // Dead renderer → handled by the heartbeat path, never input-stall.
        assert!(!should_recover_input_stall(now, 0, now - 60_000, now - 120_000, 0, true));
        // Fresh input → user is interacting (or dragging), no recovery.
        assert!(!should_recover_input_stall(now, now - 1_000, now - 5_000, now - 120_000, 0, true));
        // Stale input, but we just presented → give the user a moment.
        assert!(!should_recover_input_stall(now, now - 1_000, 0, now - 5_000, 0, true));
        // Live renderer, no input for a long time, nothing recent → recover.
        assert!(should_recover_input_stall(now, now - 1_000, now - 60_000, now - 120_000, 0, true));
        // ...but only once per cooldown.
        assert!(!should_recover_input_stall(now, now - 1_000, now - 60_000, now - 120_000, now - 10_000, true));
        // No interaction since the last recovery → never churn again.
        assert!(!should_recover_input_stall(now, now - 1_000, now - 60_000, now - 120_000, now - 120_000, false));
    }

    #[test]
    fn click_stall_recovery_fires_when_click_never_becomes_drag() {
        let now = 1_000_000u64;
        // Dead renderer → heartbeat path, never click-stall.
        assert!(!should_recover_click_stall(now, 0, now - 10_000, 0, now - 60_000, 0));
        // No click ever reached the page → nothing to detect.
        assert!(!should_recover_click_stall(now, now - 1_000, 0, 0, now - 60_000, 0));
        // Click just arrived → give the healthy page a moment to start the drag.
        assert!(!should_recover_click_stall(now, now - 1_000, now - 1_000, 0, now - 60_000, 0));
        // Click 2s old, still within grace → keep waiting.
        assert!(!should_recover_click_stall(now, now - 1_000, now - 2_000, 0, now - 60_000, 0));
        // Click 3s old with no drag → already a stall (grace is 2.5s), so it
        // beats the user's ~6s Esc dismissal.
        assert!(should_recover_click_stall(now, now - 1_000, now - 3_000, 0, now - 60_000, 0));
        // Click older than the window → stale history, don't fire.
        assert!(!should_recover_click_stall(now, now - 1_000, now - 60_000, 0, now - 120_000, 0));
        // A drag started after the press → healthy, never recover.
        assert!(!should_recover_click_stall(now, now - 1_000, now - 10_000, now - 9_000, now - 60_000, 0));
        // We just presented → let the user reach for the thumbnail.
        assert!(!should_recover_click_stall(now, now - 1_000, now - 10_000, 0, now - 1_000, 0));
        // The ghost: click received, no drag, past grace + present + cooldown → recover.
        assert!(should_recover_click_stall(now, now - 1_000, now - 10_000, 0, now - 60_000, 0));
        // ...but only once per cooldown.
        assert!(!should_recover_click_stall(now, now - 1_000, now - 10_000, 0, now - 60_000, now - 30_000));
    }

    #[test]
    fn refresh_before_present_uses_its_own_throttle() {
        let now = 1_000_000u64;
        // Long idle, never refreshed or recovered → refresh.
        assert!(should_refresh_before_present(now, now - 90_000, 90_000, 0, 0));
        // Brand-new page that never reported input → nothing to refresh.
        assert!(!should_refresh_before_present(now, 0, now, 0, 0));
        // Input is recent → warm page, no refresh.
        assert!(!should_refresh_before_present(now, now - 5_000, 5_000, 0, 0));
        // Refreshed within the cooldown (rapid captures) → no second reload.
        assert!(!should_refresh_before_present(
            now,
            now - 90_000,
            90_000,
            now - 30_000,
            0
        ));
        // Just force-recovered → page is already fresh, skip the refresh.
        assert!(!should_refresh_before_present(
            now,
            now - 90_000,
            90_000,
            0,
            now - 30_000
        ));
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
