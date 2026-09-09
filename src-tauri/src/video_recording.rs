//! Proper video recording using Windows Graphics Capture (GPU-backed) + an MP4
//! encoder via the vendored, patched `windows-capture` crate.
//!
//! See `vendor/windows-capture` for the crate-side changes. Pipeline:
//! - `VideoSession::start_free_threaded` runs WGC on its own thread and returns
//!   a `CaptureControl` that we hold in Tauri-managed state.
//! - Every frame is cropped **on the GPU** (`send_frame_region`) and handed to
//!   Media Foundation as a Direct3D surface: no readback, no row flip, no
//!   per-frame allocation. A CPU crop+flip path remains as a fallback.
//! - Timestamps come from WGC's own presentation clock, so playback spacing
//!   reflects when frames were really shown instead of a synthetic grid.
//! - The frame queue is bounded, so an encoder that falls behind sheds frames
//!   rather than building an unbounded backlog.
//! - System audio is fed by its own pump thread on its own clock, so it neither
//!   starves nor drops when the screen is static (which used to drift A/V).
//!
//! Stopping flips an `AtomicBool`; the handler sees it on its next frame and
//! calls `capture_control.stop()`. The Stop command then joins the thread,
//! stops the audio pump, and finalizes the encoder on a helper thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
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

/// Target encode bitrate for a region, in bits/second. High bitrate makes the
/// encoder fall behind real-time and drop frames (see `new`), so scale it to
/// the actual pixels and frame rate instead of the crate's 15 Mbps blanket
/// default. ~0.1 bit/pixel/frame is a strong quality bound; clamp to a sane
/// 1.5–20 Mbps so tiny and huge regions both stay reasonable.
pub fn video_bitrate_bps(w: u32, h: u32, fps: u32) -> u32 {
    let raw = (w as u64) * (h as u64) * (fps as u64) / 10; // 0.1 bit/pixel/frame
    raw.clamp(1_500_000, 20_000_000) as u32
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
    /// Capture rate (frames per second). Clamped to 5–60 in `start_recording`;
    /// threaded through so the encoder declares the SAME rate it is fed.
    pub fps: u32,
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

/// Feeds system audio to the encoder on its own clock.
///
/// WGC only delivers a frame when the screen changes, so audio used to be fed
/// solely from inside the frame callback. On a static screen nothing arrived,
/// and because the encoder's audio timeline advances by samples *sent* (while
/// the video timeline ran on wall-clock) the two drifted apart by however long
/// the screen sat still. Running the feed on its own thread keeps the audio
/// clock honest and independent of capture activity.
struct AudioPump {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    bytes: Arc<AtomicU64>,
}

impl AudioPump {
    fn start(encoder: Arc<Mutex<Option<VideoEncoder>>>, mut loopback: audio::AudioLoopback) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let bytes = Arc::new(AtomicU64::new(0));
        // Anything buffered between device start and the first video frame
        // would otherwise lead the video by that much.
        loopback.drain();
        let spawned = std::thread::Builder::new()
            .name("audio-pump".into())
            .spawn({
                let stop = stop.clone();
                let bytes = bytes.clone();
                move || audio_pump_main(encoder, loopback, stop, bytes)
            });
        match spawned {
            Ok(handle) => Self { stop, handle: Some(handle), bytes },
            Err(e) => {
                debuglog::log(&format!("video: audio pump failed to start: {e}"));
                Self { stop, handle: None, bytes }
            }
        }
    }

    /// Signals the pump to exit and waits for it. Idempotent.
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        debuglog::log(&format!(
            "video: audio pump stopped after {} bytes",
            self.bytes.load(Ordering::Relaxed)
        ));
    }
}

impl Drop for AudioPump {
    fn drop(&mut self) {
        // Join if the caller didn't stop() us, so the WASAPI thread and the
        // audio device never linger detached.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn audio_pump_main(
    encoder: Arc<Mutex<Option<VideoEncoder>>>,
    mut loopback: audio::AudioLoopback,
    stop: Arc<AtomicBool>,
    bytes: Arc<AtomicU64>,
) {
    let mut silence: Vec<u8> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let pcm = loopback.drain();
        // While paused feed NOTHING, so the audio timeline freezes alongside
        // the video one and the two stay in sync across the pause.
        if !pcm.is_empty() && !crate::toolbar::is_paused() {
            if crate::toolbar::is_muted() {
                // Silence of the same length keeps the timeline continuous (no
                // A/V drift) while producing no sound.
                if silence.len() < pcm.len() {
                    silence.resize(pcm.len(), 0);
                }
                if let Ok(mut guard) = encoder.lock() {
                    if let Some(enc) = guard.as_mut() {
                        let _ = enc.send_audio_buffer(&silence[..pcm.len()], 0);
                    }
                }
            } else if let Ok(mut guard) = encoder.lock() {
                if let Some(enc) = guard.as_mut() {
                    let _ = enc.send_audio_buffer(&pcm, 0);
                }
            }
            bytes.fetch_add(pcm.len() as u64, Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    loopback.stop();
}

/// WGC handler created fresh on the capture thread for every session.
struct VideoSession {
    /// Shared with the audio pump so PCM can be fed independently of capture.
    encoder: Arc<Mutex<Option<VideoEncoder>>>,
    /// Held until the first frame arrives, then moved into the pump thread.
    audio: Option<audio::AudioLoopback>,
    audio_pump: Option<AudioPump>,
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
    /// Accumulated "pause" wall-clock time, for the reported duration.
    paused_accum: Duration,
    /// True while a pause is in effect (this session's mirror of the global
    /// toolbar pause flag, with its transition tracked here).
    currently_paused: bool,
    /// Instant the current pause began, for computing `paused_accum` on resume.
    pause_started_at: Option<Instant>,
    /// The same pause, measured in the WGC clock (100ns ticks) so it can be
    /// subtracted from the frame timeline.
    paused_wgc: i64,
    pause_wgc_start: Option<i64>,
    /// WGC timestamp of the first emitted frame — the timeline origin.
    first_wgc_ts: Option<i64>,
    /// True until the first frame is handled; gates the audio pump start.
    first_frame: bool,
    /// False once the GPU crop fails, after which we fall back to the CPU path.
    use_gpu: bool,
    /// Requested capture rate, for the declared frame rate and the health log.
    fps: u32,
    /// Scratch buffer for the CPU fallback's top-down -> bottom-up row flip.
    /// Allocated once and resized in place.
    flip_buf: Vec<u8>,
    result: RecordingResult,
}

impl GraphicsCaptureApiHandler for VideoSession {
    type Flags = RecorderFlags;
    type Error = RecorderError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let RecorderFlags { region, monitor_rect, output_path, fps } = ctx.flags.clone();
        // Round DOWN to even; H.264/HEVC and Media Foundation require even
        // geometry. `w & !1` clears the lowest bit.
        let w = (region.right - region.left).max(2) as u32 & !1;
        let h = (region.bottom - region.top).max(2) as u32 & !1;
        debuglog::log(&format!("video: handler new {}x{} @{}fps -> {}", w, h, fps, output_path));
        // Declare the SAME rate we feed. The builder's default is 60fps; if we
        // leave it, the MP4/encoder think the stream is 60fps while we send 30
        // frames/sec and players read that back as stutter.
        let encoder = VideoEncoder::new(
            VideoSettingsBuilder::new(w, h)
                .sub_type(VideoSettingsSubType::H264)
                .frame_rate(fps)
                // Don't trust the 15 Mbps default (geared to 1080p60+). A
                // bitrate far above a region's needs makes the encoder work
                // harder per frame and it falls behind real-time, so the
                // bounded queue starts shedding. Scale to the actual pixels
                // and rate: ~0.1 bit/pixel/frame is a high-quality bound,
                // clamped to a sane 1.5–20 Mbps.
                .bitrate(video_bitrate_bps(w, h, fps)),
            // System audio is enabled; PCM is fed continuously by the audio
            // pump. 48 kHz / stereo / 16-bit is the encoder default and matches
            // what `audio::AudioLoopback` delivers.
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
            encoder: Arc::new(Mutex::new(Some(encoder))),
            audio,
            audio_pump: None,
            region,
            monitor_rect,
            enc_w: w,
            enc_h: h,
            output_path,
            started: Instant::now(),
            frames: 0,
            stop_time: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
            paused_accum: Duration::ZERO,
            currently_paused: false,
            pause_started_at: None,
            paused_wgc: 0,
            pause_wgc_start: None,
            first_wgc_ts: None,
            first_frame: true,
            use_gpu: true,
            fps,
            flip_buf: Vec::new(),
            result: RecordingResult::default(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Ground truth for when this frame was presented (QPC-derived 100ns
        // ticks). The old code threw this away and reconstructed a timeline
        // from wall-clock at push time, which baked every scheduler hiccup and
        // driver stall into the playback spacing.
        let wgc_ts = frame.timestamp()?.Duration;

        // Track pause transitions from the global toolbar flag, in BOTH clocks:
        // wall-clock for the reported duration, WGC time for the frame
        // timeline. While paused we feed the encoder nothing, so both timelines
        // freeze together and stay in sync.
        let pause_req = crate::toolbar::is_paused();
        if pause_req && !self.currently_paused {
            self.currently_paused = true;
            self.pause_started_at = Some(Instant::now());
            self.pause_wgc_start = Some(wgc_ts);
        } else if !pause_req && self.currently_paused {
            if let Some(ps) = self.pause_started_at.take() {
                self.paused_accum += ps.elapsed();
            }
            if let Some(pw) = self.pause_wgc_start.take() {
                self.paused_wgc += wgc_ts.saturating_sub(pw);
            }
            self.currently_paused = false;
        }
        // Paused: drop this frame entirely (video + audio both frozen). Still
        // honor a stop request so the toolbar/tray Stop works mid-pause.
        if self.currently_paused {
            if self.stop_requested.load(Ordering::SeqCst) {
                capture_control.stop();
            }
            return Ok(());
        }

        // Timeline: real elapsed presentation time since the first frame, with
        // paused spans removed so the file simply omits them. Frames land
        // wherever they actually occurred — no grid snapping, so a late frame
        // reads as a late frame instead of a doubled gap.
        let base = *self.first_wgc_ts.get_or_insert(wgc_ts);
        let ts = wgc_ts.saturating_sub(base).saturating_sub(self.paused_wgc).max(0);

        // Crop origin, relative to the captured monitor's top-left.
        let ox = (self.region.left - self.monitor_rect.left).max(0) as u32;
        let oy = (self.region.top - self.monitor_rect.top).max(0) as u32;

        let mut sent = false;
        {
            let mut guard = self.encoder.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(enc) = guard.as_mut() {
                if self.use_gpu {
                    // `send_frame_region` reports Ok even when backpressure
                    // shed the sample, so compare the shed counter to learn
                    // whether this frame actually reached the encoder.
                    // Counting shed frames as sent would make the health log
                    // report the target rate while the file quietly misses
                    // them — hiding the very problem we're measuring.
                    let before = enc.dropped_frames();
                    match enc.send_frame_region(frame, ox, oy, ts) {
                        Ok(()) => sent = enc.dropped_frames() == before,
                        Err(e) => {
                            debuglog::log(&format!(
                                "video: GPU crop failed ({e}); falling back to the CPU path"
                            ));
                            self.use_gpu = false;
                        }
                    }
                }
                if !sent && ox + self.enc_w <= frame.width() && oy + self.enc_h <= frame.height() {
                    // CPU fallback: read the crop back and flip it bottom-up.
                    // (Only the raw-buffer path needs that flip — the surface
                    // path above does not.) Regions that overhang the capture
                    // texture can't be padded cheaply here, and the GPU path
                    // already handles them, so those frames are skipped.
                    let buf = frame.buffer_crop(ox, oy, ox + self.enc_w, oy + self.enc_h)?;
                    let h = buf.height();
                    let mut no_pad: Vec<u8> = Vec::new();
                    let bgra = buf.as_nopadding_buffer(&mut no_pad);
                    let row = buf.width() as usize * 4;
                    self.flip_buf.resize(bgra.len(), 0);
                    for y in 0..h as usize {
                        let src = &bgra[y * row..(y + 1) * row];
                        let dst_row = (h as usize - 1 - y) * row;
                        self.flip_buf[dst_row..dst_row + row].copy_from_slice(src);
                    }
                    enc.send_frame_buffer(&self.flip_buf, ts)?;
                    sent = true;
                }
            }
        }
        if sent {
            self.frames += 1;

            // Periodic health check. The whole point of the GPU path is a
            // capture loop that keeps up, so surface the achieved rate and any
            // backpressure shedding where it can actually be read. If
            // "fps effective" sits well under the target, the encoder (not the
            // capture) is the limit; if "shed" climbs, it has fallen behind.
            if self.frames % 150 == 0 {
                let secs = (self.started.elapsed() - self.paused_accum).as_secs_f64();
                let rate = if secs > 0.0 { self.frames as f64 / secs } else { 0.0 };
                debuglog::log(&format!(
                    "video: health — {} frames, {:.1} fps effective (target {}), {} shed by backpressure",
                    self.frames,
                    rate,
                    self.fps,
                    self.dropped_count()
                ));
            }
        }

        // Open the audio gate on the first frame, so the audio clock starts
        // together with the video timeline rather than at device-open time.
        if self.first_frame {
            self.first_frame = false;
            if let Some(lb) = self.audio.take() {
                self.audio_pump = Some(AudioPump::start(self.encoder.clone(), lb));
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
    /// Frames the bounded queue shed because the encoder fell behind.
    fn dropped_count(&self) -> u64 {
        match self.encoder.lock() {
            Ok(g) => g.as_ref().map(VideoEncoder::dropped_frames).unwrap_or(0),
            Err(p) => p.into_inner().as_ref().map(VideoEncoder::dropped_frames).unwrap_or(0),
        }
    }

    /// Finalize the encoder and produce the result. Idempotent: a repeat call
    /// returns the already-computed result, and a call when nothing was taken
    /// is a no-op. The result is RETURNED (not left for the caller to read back
    /// through the session lock), so callers never re-lock the session while a
    /// possibly-hung finalize is in flight — that blocking lock read is what
    /// defeated the earlier timeout. Runs on whatever thread calls this; keep
    /// it off the main/UI thread.
    fn finalize(&mut self) -> RecordingResult {
        // Stop the audio feed FIRST: its thread must be joined and torn down
        // even if the (GPU/Media Foundation) encoder finalize below hangs, so a
        // stalled finalize can never leak the WASAPI thread. Safe because the
        // WGC capture thread is already joined at this point.
        if let Some(mut pump) = self.audio_pump.take() {
            pump.stop();
        }
        if let Some(mut a) = self.audio.take() {
            // Never handed to a pump (no frames arrived at all).
            a.stop();
        }
        // How many frames the bounded queue shed because the encoder lagged.
        let dropped = self.dropped_count();
        let mut guard = self.encoder.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(enc) = guard.take() {
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
                // Exclude paused wall-clock time so the reported duration
                // reflects the actual recording (the file omits the pause).
                elapsed_ms: self
                    .stop_time
                    .map(|t| (t.saturating_duration_since(self.started) - self.paused_accum).as_millis())
                    .unwrap_or_else(|| (self.started.elapsed() - self.paused_accum).as_millis()),
                width: self.enc_w,
                height: self.enc_h,
            };
            debuglog::log(&format!(
                "video: finished {} frames in {}ms ({} shed by backpressure) -> {}",
                self.result.frames, self.result.elapsed_ms, dropped, self.result.path
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
    // Throttle WGC to the requested rate (clamped to a sane range). This keeps
    // the capture/encode loop inside its real-time budget on high-refresh
    // displays, where WGC would otherwise offer frames at 60–144 Hz.
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

    let flags = RecorderFlags { region, monitor_rect, output_path: path.clone(), fps };

    let settings = Settings::new(
        item_monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        // Nanoseconds, not milliseconds: `from_millis(1000 / 30)` truncates to
        // 33ms (30.3fps) and quietly desynchronises the requested rate from the
        // rate the encoder is told to expect.
        MinimumUpdateIntervalSettings::Custom(Duration::from_nanos(1_000_000_000 / fps as u64)),
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

/// Start, run and stop a recording without a Tauri app.
///
/// Used by the `video_smoke` example (re-exported through `test_helpers`) to
/// exercise the real capture -> GPU crop -> encode -> audio pump -> finalize
/// pipeline headlessly. Everything below `start_free_threaded` is production
/// code; only the Tauri window/tray/toolbar orchestration is skipped.
pub fn record_headless(
    region: RECT,
    monitor: Monitor,
    path: String,
    fps: u32,
    secs: u64,
    log_path: Option<String>,
) -> Result<RecordingResult, String> {
    if let Some(p) = log_path {
        crate::debuglog::init_path(std::path::PathBuf::from(p));
    }
    let fps = fps.clamp(5, 60);
    let mw = monitor.width().map_err(|e| e.to_string())? as i32;
    let mh = monitor.height().map_err(|e| e.to_string())? as i32;
    let monitor_rect = RECT { left: 0, top: 0, right: mw, bottom: mh };

    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::Default,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(Duration::from_nanos(1_000_000_000 / fps as u64)),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        RecorderFlags { region, monitor_rect, output_path: path.clone(), fps },
    );

    let control = VideoSession::start_free_threaded(settings).map_err(|e| e.to_string())?;
    ACTIVE.store(true, Ordering::SeqCst);
    debuglog::log(&format!("video: headless start -> {path}"));

    std::thread::sleep(Duration::from_secs(secs));

    let session = control.callback();
    {
        let mut cb = session.lock();
        cb.stop_requested.store(true, Ordering::SeqCst);
        cb.stop_time = Some(Instant::now());
    }
    let _: Result<(), _> = control.stop();
    debuglog::log("video: headless — capture thread joined");
    let result = session.lock().finalize();
    ACTIVE.store(false, Ordering::SeqCst);
    Ok(result)
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
    // hung finalize holds). Now that the frame queue is bounded there is far
    // less left to drain, so this is normally well under a second.
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
