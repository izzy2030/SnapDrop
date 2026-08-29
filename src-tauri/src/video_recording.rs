//! Proper video recording using Windows Graphics Capture (GPU-backed) + an MP4
//! encoder via the `windows-capture` crate.
//!
//! Design:
//! - `VideoSession::start_free_threaded` runs WGC on its own thread and returns
//!   a `CaptureControl` that we hold in Tauri-managed state.
//! - Every WGC frame is cropped to the selected region (`Frame::buffer_crop`),
//!   converted from RGBA/top-down to BGRA/bottom-to-top, and pushed to a
//!   `VideoEncoder` writing an MP4 file.
//! - Stopping flips an `AtomicBool`; the handler sees it on its next frame,
//!   finalizes the encoder, records the result, and calls
//!   `capture_control.stop()`. The Stop command then joins the thread and reads
//!   the result back via `CaptureControl::callback()`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::Manager;
use windows::Win32::Foundation::RECT;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, VideoEncoder, VideoSettingsBuilder, VideoSettingsSubType,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::monitor::Monitor;

use crate::{audio, debuglog, notifier};

type RecorderError = Box<dyn std::error::Error + Send + Sync>;

/// Cheap global "is a recording active" flag, read by the tray without touching
/// managed state (avoids the `State` borrow-lifetime tango).
static ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_recording_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// Flags threaded into the handler via `Settings`. Must be `Send`; it uses
/// plain `RECT`s (never the non-`Send` `HMONITOR`) plus the output path as a
/// `String` (which is `Send`). Delivered to the *capture thread* inside the
/// `Context`, so the handler reads the path from here — never from a
/// per-thread value that wouldn't be visible.
#[derive(Debug, Clone)]
pub struct RecorderFlags {
    /// The region to record, in physical virtual-screen px.
    pub region: RECT,
    /// Physical rect of the monitor being captured, for crop offsets.
    pub monitor_rect: RECT,
    /// Full path of the MP4 to write.
    pub output_path: String,
}

/// Outcome of a finished recording session.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RecordingResult {
    pub path: String,
    pub frames: u64,
    pub elapsed_ms: u128,
    /// Recorded region dimensions (even-adjusted), for the history entry.
    pub width: u32,
    pub height: u32,
}


/// WGC handler created fresh on the capture thread for every session.
struct VideoSession {
    encoder: Option<VideoEncoder>,
    /// System-audio loopback capture (output mix). `None` if no audio device
    /// was available; recording proceeds without sound.
    audio: Option<audio::AudioLoopback>,
    region: RECT,
    monitor_rect: RECT,
    /// Even encoder/crop width (H.264/Media Foundation needs even dims).
    enc_w: u32,
    /// Even encoder/crop height.
    enc_h: u32,
    output_path: String,
    started: Instant,
    frames: u64,
    /// Set when the stop signal is flipped, so duration excludes the
    /// stop+finalize latency (the encoder is finaled later, on another thread).
    stop_time: Option<Instant>,
    stop_requested: Arc<AtomicBool>,
    /// True until the first frame is sent — used to drop the PCM that
    /// accumulated between audio start and the first video frame.
    first_frame: bool,
    result: RecordingResult,
}

impl GraphicsCaptureApiHandler for VideoSession {
    type Flags = RecorderFlags;
    type Error = RecorderError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let RecorderFlags { region, monitor_rect, output_path } = ctx.flags.clone();
        // Round DOWN to even; H.264/HEVC and Media Foundation require even
        // geometry. `w & !1` clears the lowest bit.
        let w = (region.right - region.left).max(2) as u32 & !1;
        let h = (region.bottom - region.top).max(2) as u32 & !1;
        debuglog::log(&format!("video: handler new {}x{} -> {}", w, h, output_path));
        let encoder = VideoEncoder::new(
            VideoSettingsBuilder::new(w, h).sub_type(VideoSettingsSubType::H264),
            // System audio is enabled; PCM is fed continuously from the WASAPI
            // loopback capture. 48 kHz / stereo / 16-bit is the encoder default
            // and matches what `audio::AudioLoopback` delivers.
            AudioSettingsBuilder::default(),
            ContainerSettingsBuilder::default(),
            &output_path,
        )?;
        let audio = audio::AudioLoopback::start();
        debuglog::log(&format!(
            "video: audio capture {}",
            if audio.is_some() { "ready" } else { "unavailable (recording silent)" }
        ));
        Ok(VideoSession {
            encoder: Some(encoder),
            audio,
            region,
            monitor_rect,
            enc_w: w,
            enc_h: h,
            output_path,
            started: Instant::now(),
            frames: 0,
            stop_time: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            first_frame: true,
            result: RecordingResult::default(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Timestamp must be read before the (mutating) crop below borrows the frame.
        let ts = frame.timestamp().map(|t| t.Duration).unwrap_or(0) as i64;
        // Crop to the region (rounded to even dimensions), relative to the monitor top-left.
        let ox = (self.region.left - self.monitor_rect.left).max(0) as u32;
        let oy = (self.region.top - self.monitor_rect.top).max(0) as u32;
        let ex = ox + self.enc_w;
        let ey = oy + self.enc_h;
        let buf = frame.buffer_crop(ox, oy, ex, ey)?;
        let h = buf.height();
        let mut no_pad: Vec<u8> = Vec::new();
        let bgra = buf.as_nopadding_buffer(&mut no_pad);
        let row = buf.width() as usize * 4;
        // Capture is now requested as Bgra8, so no per-pixel channel swap is
        // needed. We only flip rows top-down -> bottom-to-top (which Windows /
        // MF expect). ImageMemReader returns a wrapper that lets us iterate
        // without re-owning every byte; flip in place with a row-by-row swap.
        let mut flipped = vec![0u8; bgra.len()];
        for y in 0..h as usize {
            let src = &bgra[y * row..(y + 1) * row];
            let dst_row = (h as usize - 1 - y) * row;
            flipped[dst_row..dst_row + row].copy_from_slice(src);
        }
        if let Some(enc) = self.encoder.as_mut() {
            enc.send_frame_buffer(&flipped, ts)?;
            self.frames += 1;
            // Feed whatever system audio accumulated since the last frame.
            // When muted, send silence of the same length so the audio
            // timeline stays continuous (no A/V drift) but produces no sound.
            if let Some(a) = self.audio.as_mut() {
                let pcm = a.drain();
                if self.first_frame {
                    // Drop PCM buffered between audio start and the first video
                    // frame so the audio track doesn't lead the video by a
                    // frame or two (the WASAPI loopback starts in `new`, before
                    // the first WGC frame lands).
                    self.first_frame = false;
                } else if !pcm.is_empty() {
                    if crate::toolbar::is_muted() {
                        enc.send_audio_buffer(&vec![0u8; pcm.len()], 0)?;
                    } else {
                        enc.send_audio_buffer(&pcm, 0)?;
                    }
                }
            }
        }

        // Respond to an externally requested stop: just end the WGC thread.
        // Encoder finalization happens in `stop_recording` AFTER this thread
        // joins, so a slow/blocked transcoder never stalls the capture loop
        // (which `control.stop()` in the caller would otherwise join -> freeze).
        if self.stop_requested.load(Ordering::SeqCst) {
            capture_control.stop();
            return Ok(());
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        // Deliberately do NOT finalize here: `enc.finish()` blocks on the
        // Media Foundation transcoder join, which can hang for a long time on
        // a GPU/driver stall. Finalizing on this (WGC) thread would make
        // `control.stop()`'s join hang and freeze the app at Stop. The stop
        // path finalizes AFTER this thread has joined, with a timeout.
        debuglog::log("video: wgc thread closed (finalize deferred to stop path)");
        Ok(())
    }
}


impl VideoSession {
    /// Finalize the encoder and produce the result. Idempotent: a repeat call
    /// returns the already-computed result, and a call when nothing was taken
    /// is a no-op. The result is RETURNED (not left for the caller to read back
    /// through the session lock), so callers never re-lock the session while a
    /// possibly-hung finalize is in flight — that blocking lock read is what
    /// defeated the earlier timeout. Runs on whatever thread calls this; keep
    /// it off the main/UI thread.
    fn finalize(&mut self) -> RecordingResult {
        if let Some(enc) = self.encoder.take() {
            // Stop the audio capture FIRST: its thread must be joined and torn
            // down even if the (GPU/Media Foundation) encoder finalize below
            // hangs, so a stalled finalize can never leak the WASAPI thread.
            // Safe because the WGC capture thread is already joined at this
            // point — all PCM was already fed, so nothing is lost.
            if let Some(a) = self.audio.take() {
                let mut a = a;
                a.stop();
                debuglog::log(&format!("video: audio captured {} bytes", a.bytes_captured()));
            }
            if let Err(e) = enc.finish() {
                debuglog::log(&format!("video: encoder finish error: {e}"));
                // A failed finish leaves the file truncated/unfinalized — don't
                // present it as a successful recording (history is gated on
                // frames > 0).
                self.frames = 0;
            }
            self.result = RecordingResult {
                path: self.output_path.clone(),
                frames: self.frames,
                elapsed_ms: self
                .stop_time
                .map(|t| t.duration_since(self.started).as_millis())
                .unwrap_or_else(|| self.started.elapsed().as_millis()),
                width: self.enc_w,
                height: self.enc_h,
            };
            debuglog::log(&format!(
                "video: finished {} frames in {}ms -> {}",
                self.result.frames, self.result.elapsed_ms, self.result.path
            ));
        }
        self.result.clone()
    }
}

/// A region selected for recording but not yet started (waiting for the user
/// to hit Rec on the floating toolbar).
#[derive(Debug, Clone)]
pub struct PendingRecording {
    pub region: RECT,
    pub path: String,
    /// Main-window state captured by `hide_for_capture` before the selection
    /// overlay; used to restore it correctly if the user cancels.
    pub was_visible: bool,
    pub was_minimized: bool,
}

/// Tauri-managed state holding the active capture control, the last result,
/// and any armed-but-not-started recording.
pub struct VideoRecorder {
    control: Option<CaptureControl<VideoSession, RecorderError>>,
    pending: Option<PendingRecording>,
    /// True from when a start is reserved until the control is stored, so two
    /// concurrent starts can't both pass the "already active" check and orphan
    /// a live WGC session (the start TOCTOU).
    starting: bool,
}

impl VideoRecorder {
    pub fn new() -> Self {
        Self { control: None, pending: None, starting: false }
    }
    pub fn is_recording(&self) -> bool {
        self.control.is_some()
    }
    /// Atomically reserve the start slot. Errors if a recording is already
    /// active or another start is already being set up.
    pub fn try_reserve_start(&mut self) -> Result<(), String> {
        if self.control.is_some() {
            return Err("a recording is already active".into());
        }
        if self.starting {
            return Err("a recording is already starting".into());
        }
        self.starting = true;
        Ok(())
    }
    /// Arm a recording (region + MP4 path) from the selection flow; started
    /// later via `take_pending` when the user clicks Rec.
    pub fn arm(&mut self, region: RECT, path: String, was_visible: bool, was_minimized: bool) {
        self.pending = Some(PendingRecording { region, path, was_visible, was_minimized });
    }
    pub fn take_pending(&mut self) -> Option<PendingRecording> {
        self.pending.take()
    }
}

/// Start recording `region` (physical virtual-screen px) to `path`. Captures
/// the monitor containing the region and crops each frame to it.
pub fn start_recording(
    app: &tauri::AppHandle,
    region: RECT,
    path: String,
    fps: u32,
) -> Result<String, String> {
    // Throttle WGC to the requested rate (clamped to a sane range). This is
    // the main lever against the stutter: without it we capture+convert+frame
    // at 60–144 Hz and the MF encoder can't keep real-time pace.
    let fps = fps.clamp(5, 60);
    {
        let rec_state = app.state::<Mutex<VideoRecorder>>();
        let mut rec = rec_state.lock().unwrap();
        // Reserve the start slot under the lock BEFORE any session/thread work,
        // so a concurrent start cannot pass the active-check and clobber this
        // (losing) live session before its control is stored.
        rec.try_reserve_start()?;
    }

    let cx = region.left + (region.right - region.left) / 2;
    let cy = region.top + (region.bottom - region.top) / 2;
    let monitor = crate::monitors::monitor_at_point(cx, cy)
        .ok_or_else(|| "region is not on any monitor".to_string())?;
    let monitor_rect = monitor.rect;
    // HMONITOR is `*mut c_void`; Monitor wraps it and is `unsafe impl Send`.
    let item_monitor = Monitor::from_raw_hmonitor(monitor.hmonitor.0);

    let flags = RecorderFlags { region, monitor_rect, output_path: path.clone() };

    let settings = Settings::new(
        item_monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(Duration::from_millis(1000 / fps as u64)),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );

    match VideoSession::start_free_threaded(settings) {
        Ok(control) => {
            // Store the control under the lock, then DROP it immediately: the
            // window/tray/toolbar ops below dispatch back onto the main thread,
            // and holding managed state across them is the same lock-order
            // hazard fixed on the stop path.
            {
                let rec_state = app.state::<Mutex<VideoRecorder>>();
                let mut rec = rec_state.lock().unwrap();
                rec.control = Some(control);
                rec.starting = false;
            }
            ACTIVE.store(true, Ordering::SeqCst);
            // SnapDrop must never film itself: hide the app's windows before
            // the first frame lands so they don't appear in the recording. The
            // tray gains a "Stop Recording" item so the user can end the session.
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.hide();
            }
            let _ = crate::thumbnail::hide_all(app);
            crate::tray::refresh(app);
            // Dashed border around the recorded region + toolbar in recording
            // mode (timer + mute + stop). Border first so the toolbar wins the
            // z-order where they overlap.
            crate::toolbar::show_border(app, region);
            crate::toolbar::show_recording(app);
            debuglog::log(&format!("video: started -> {path}"));
            Ok(path)
        }
        Err(e) => {
            // Release the start reservation so the app stays usable. The caller
            // restores the main window from the armed pending state.
            let rec_state = app.state::<Mutex<VideoRecorder>>();
            let mut rec = rec_state.lock().unwrap();
            rec.starting = false;
            Err(e.to_string())
        }
    }
}

/// Stop the active recording. The handler finalizes on its next frame and
/// stops the WGC thread; we join it and return the result.
pub fn stop_recording(app: &tauri::AppHandle) -> Result<RecordingResult, String> {
    // Take the control under the lock, then DROP the lock immediately. The
    // rest of the stop (join, finalize, history registration, window/tray
    // ops) must never run while holding managed state: those ops dispatch
    // events and commands that can run on the main thread, and holding a
    // state lock across them creates lock-order hazards (the crash/stall
    // seen when history registration was added while the lock was held).
    let control = {
        let rec_state = app.state::<Mutex<VideoRecorder>>();
        let mut rec = rec_state.lock().unwrap();
        let Some(control) = rec.control.take() else {
            return Err("no recording is active".into());
        };
        control
    };
    debuglog::log("video: stop — joining capture thread");
    // Hold our own clone of the shared callback so it stays alive even after
    // `control.stop()` consumes the control. The handler's `result` lives on
    // that shared object.
    let session = control.callback();
    {
        let mut cb = session.lock();
        cb.stop_requested.store(true, Ordering::SeqCst);
        cb.stop_time = Some(Instant::now());
    }
    // Posts WM_QUIT and joins the capture thread. If the handler already
    // stopped itself, this still joins cleanly. Only AFTER the thread has
    // fully joined do we finalize the encoder (which blocks on the transcoder) —
    // this must never run on the UI/main thread.
    let _: Result<(), _> = control.stop();
    debuglog::log("video: stop — capture thread joined");
    // Snapshot path/dims now (the session lock is free at this moment) so we
    // can still produce a meaningful result if finalize times out below.
    let (fallback_path, fallback_w, fallback_h) = {
        let s = session.lock();
        (s.output_path.clone(), s.enc_w, s.enc_h)
    };
    // Finalize on a helper thread so a hung encoder/transcoder (e.g. a GPU
    // driver stall) can never freeze the app at Stop. finalize() RETURNS the
    // result over the channel, so on timeout we fall back WITHOUT re-locking
    // the session (the old `session.lock().result.clone()` at this point is
    // what defeated the earlier timeout — it blocked on the same mutex the
    // hung finalize holds). Normally finishes in ~1–2s; give it a budget.
    let session_fin = session.clone();
    let (tx, rx) = std::sync::mpsc::channel::<RecordingResult>();
    std::thread::spawn(move || {
        let r = session_fin.lock().finalize();
        let _ = tx.send(r);
    });
    let result = match rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(r) => {
            debuglog::log(&format!("video: stop — encoder finalized {:?}", r));
            r
        }
        Err(_) => {
            notifier::toast(
                app,
                "error",
                "Recording stopped but the encoder stalled — the file may be incomplete.",
            );
            // The finalize may be merely slow (a long transcoder join) rather
            // than truly hung. If the file already has content on disk, don't
            // silently lose it: register it in history (frames > 0) so the user
            // can open/reveal/delete it. If the encoder is genuinely wedged the
            // file is truncated and we skip it.
            let size = std::fs::metadata(&fallback_path)
                .map(|m| m.len())
                .unwrap_or(0);
            if size > 0 {
                debuglog::log(&format!(
                    "video: stop — finalize timed out but {fallback_path} exists ({size} bytes); registering anyway"
                ));
                RecordingResult {
                    path: fallback_path,
                    frames: 1,
                    elapsed_ms: 0,
                    width: fallback_w,
                    height: fallback_h,
                }
            } else {
                debuglog::log(
                    "video: stop — WARNING encoder finalize timed out (GPU/encoder stall); file absent, skipped",
                );
                RecordingResult {
                    path: fallback_path,
                    frames: 0,
                    elapsed_ms: 0,
                    width: fallback_w,
                    height: fallback_h,
                }
            }
        }
    };
    // Register the finished video in the app history (like screenshots), so it
    // shows up in the gallery with duration/size and can be opened, revealed,
    // dragged, or deleted from there. Skip empty/failed sessions.
    if result.frames > 0 && !result.path.is_empty() {
        crate::history::add_video(
            app,
            result.path.clone(),
            chrono::Local::now().to_rfc3339(),
            (result.elapsed_ms / 1000).max(1) as u64,
            result.width,
            result.height,
        );
        debuglog::log(&format!("video: added to history -> {}", result.path));
    }
    // If a NEW recording started while we were stopping, leave its windows,
    // toolbar, border and ACTIVE flag untouched — restoring ours would make
    // SnapDrop film itself and disable the live session's guards.
    let still_idle = {
        let rec_state = app.state::<Mutex<VideoRecorder>>();
        let rec = rec_state.lock().unwrap();
        // Treat a start that is reserved-but-not-yet-stored as "active" too, so
        // a stop finishing during a concurrent start's setup doesn't yank the
        // app window back on screen (SnapDrop would film itself).
        rec.control.is_none() && !rec.starting
    };
    if !still_idle {
        debuglog::log("video: stop — a new recording is live; skipping window/toolbar teardown");
        return Ok(result);
    }
    ACTIVE.store(false, Ordering::SeqCst);
    // Bring the app's window back now that the recording is over.
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
    }
    debuglog::log("video: stop — window restored, refreshing tray");
    crate::tray::refresh(app);
    crate::toolbar::hide(app);
    crate::toolbar::hide_border(app);
    debuglog::log(&format!("video: stopped -> {:?}", result));
    Ok(result)
}