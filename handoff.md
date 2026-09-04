# Handoff — Video Recording Feature

Written 2026-08-29 at commit `5bd2d33`. A "get productive in 2 seconds" guide
for an agent (or human) picking up SnapDrop's screen-recording feature.

---

## TL;DR

**Stack:** Tauri 2 (Rust backend + React/Vite frontend) on Windows. Video is
captured with **Windows Graphics Capture** (via the `windows-capture` crate),
encoded to **MP4/H.264** with **Media Foundation**, and system audio captured
via **WASAPI loopback**. 100% local, no cloud.

**Default hotkey:** `Ctrl+Alt+V` → draw a region → floating toolbar → records to
your Screenshot folder as `SnapDrop_video_<timestamp>.mp4`.

**To run:** `npm run tauri dev` (backend auto-recompiles on save; Vite HMR for
the frontend). One important environment note from real experience: if the
recorder windows ever render a *stale mirror of the app UI* or fail to appear,
first clear the WebView2 cache folder `%LOCALAPPDATA%\com.snapdrop.desktop\EBWebView`
(back it up, the app regenerates it). This is an environmental fix, **not** a
code change.

---

## Getting oriented (2-second map)

Everything for recording is in the Rust backend plus a couple of small
frontend entry points:

| Concern | File | Role |
|---|---|---|
| Record loop + encoder + CFR timestamps | `src-tauri/src/video_recording.rs` | Core: WGC → crop/flip → MP4 encode, audio feed |
| System-audio capture | `src-tauri/src/audio.rs` | WASAPI loopback → PCM → encoder |
| Toolbar/border window control | `src-tauri/src/toolbar.rs` | `WDA_EXCLUDEFROMCAPTURE`, position, arm/record/pause/mute state |
| Region selection flow (Ctrl+Alt+V) | `src-tauri/src/capture_flow.rs` | `run_video` — hide main window, run overlay, arm recorder |
| Tauri commands (invoke from JS) | `src-tauri/src/commands.rs` | `video_record_begin/stop`, `video_toggle_mute`, `video_toggle_pause`, preview |
| Hotkey registration | `src-tauri/src/hotkey.rs` | `init_video_hotkey`, `apply_video_settings` |
| Settings (fps, hotkey) | `src-tauri/src/settings.rs` | `video_fps` (default 30), `video_hotkey` |
| Visual invitation/hints | `src-tauri/src/tray.rs` | adds "Stop Recording" while active |
| Toolbar UI | `src/recorder.tsx` + `recorder.html` | Rec/Mute/Pause/Stop buttons, timer |
| Region border UI | `src/recorder_border.tsx` + `recorder_border.html` | dashed border + size + drag handles |

Frontend entries are wired in `vite.config.ts` (`rollupOptions.input`) and the
windows in `src-tauri/tauri.conf.json` (`recorder_toolbar`, `recorder_border`).

---

## Work flow (what happens on Ctrl+Alt+V)

1. `init_video_hotkey` registers the global hotkey (`hotkey.rs`).
2. Handler → `capture_flow::run_video`:
   - bails if a recording is already active;
   - **hides the main SnapDrop window** so the app can't film itself;
   - runs the native Win32 selection overlay (`overlay::run`);
   - if the user cancels → restores the window and stops;
   - otherwise saves the region as the "last area", builds an output path, and
     calls `VideoRecorder::arm(...)` + `toolbar::arm` + `toolbar::show_border`.
3. Toolbar shows **Rec / Mute / Cancel**. Clicking **Rec** invokes
   `video_record_begin` → `start_recording(...)` → WGC session starts on its
   own thread.
4. **Stop** (toolbar or tray) → `video_record_stop` → finalize the MP4,
   write history, restore the main window, emit `history-updated`.
5. Video lands in history with a real frame thumbnail (built
   in `commands::build_capture_preview`), double-click opens the default player.

---

## Key design decisions (don't "fix" these — understand them first)

**Constant frame rate (CFR) timestamps.** We stamp every frame onto the exact
grid `n × (1s ÷ fps)`, not WGC's raw arrival time (which is jittery: 33/34/67ms).
This is what removes visible stutter. When we fall more than one frame behind
wall-clock (a real encoder/GPU stall), we snap forward to re-sync rather than
keep `nominal += 1`, so audio (which uses its own monotonic clock) never drifts.
See the comments around the timestamping in `video_recording.rs`.

**`video_fps` is default 30, not 60.** Empirically, Media Foundation's H.264
encoder on this machine tops out at ~28–30 fps *real-time* at this resolution,
even when asked for 60 (we measured `364 frames / 12.2s ≈ 30fps` at `@60fps`).
Declaring 60 while feeding ~30 samples/33ms produced the same "declared vs fed"
mismatch that caused the original stutter. So: default 30, selectable to 60 in
Settings for machines that can sustain it. The `start_recording` clamps `fps`
to **5–60**.

**Resolution-aware bitrate.** The crate defaults to 15 Mbps (geared to
1080p60+). `video_bitrate_bps(w, h, fps)` scales to ~0.1 bit/pixel/frame,
clamped 1.5–20 Mbps — e.g. ~6.8 Mbps for a 1522×744@60 region. Lower encode
cost = the encoder keeps real-time pace better.

**Even dimensions.** Video dims are forced even (`& !1`) and the crop rect is
rounded down to exactly the encoder dims — otherwise Media Foundation throws
`MF_E_INVALIDMEDIATYPE` on odd regions.

**Mute keeps the audio timeline continuous.** Instead of dropping PCM, mute
zeroes each buffer (`vec![0u8; pcm.len()]`) per frame burst. The audio
timeline stays sample-counted, so toggling mute mid-recording doesn't cause
A/V drift.

**Pause freezes BOTH timelines.** A `PAUSED` atomic in `toolbar.rs` makes the
handler feed the encoder nothing (no video, no audio) while paused, then
subtracts the paused wall-clock time on resume. Output simply omits the paused
period with no hole or dead air. The flag is reset to false at the start of
every new recording.

**Capture/hide self-exclusion.** The main window hides before recording, and
the toolbar + border windows get `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`
so the app never films itself. Border is click-through
(`set_ignore_cursor_events(true)`).

---

## Gotchas / hard-won lessons

- **Transparent WebView2 windows + stale cache.** If `recorder_toolbar` /
  `recorder_border` ever show a *frozen mirror of the app UI* instead of their
  own content, or don't appear at all with `WebView2 error 0x8007139F`:
  1. Clear `%LOCALAPPDATA%\com.snapdrop.desktop\EBWebView` (regenerates).
  2. If truly stuck, update the WebView2 runtime. (The 0x8007139F was caused
     by adding `--disable-gpu-compositing` via `additionalBrowserArgs` — that
     forces a second WebView2 environment on the same data folder, which
     Windows refuses. Don't reintroduce that per-window flag.)
- **Encoder is the bottleneck.** WGC delivers easily; MF encode is what caps
  you ~30fps. Don't chase "60fps" on this machine — it wastes CPU and, with
  CFR timestamps at 33ms, declares a rate the encoder can't feed. A hardware
  encoder (NVENC/QSV) is the real path to 60fps, if ever needed.
- **Stopping must not block the WGC/graphics thread.** `video_record_stop` runs
  via `spawn_blocking`, takes the encoder out, and finalizes on a helper thread
  with a 20s timeout so a GPU stall can't wedge the main loop / app ("Not
  Responding"). Keep finalize off the capture thread and off the main thread.
- **The debug log is your friend.** `[...]/com.snapdrop.desktop/snapdrop-debug.log`
  logs the whole video lifecycle (`video capture flow: start`, `armed region
  ... -> path`, `video: started ->`, `video: stop`, `video: added to history`).
  Also visible in-app (Settings → log viewer). Use it to see frame counts
  (`finished N frames in ...ms`) to validate fps targets.
- **The log ROTATES, never truncates.** On each start the previous session's
  log moves to `snapdrop-debug.prev.log` (visible via Settings → log viewer →
  "View previous session log"). A wedged-but-alive app (black window, missing
  thumbnail, frozen task manager) never writes `snapdrop_panic.txt` — the
  debug log is the ONLY evidence, so never delete the `.prev` file before
  diagnosing. Startup now also logs session context (`startup context: ...`:
  monitors, virtual screen, DWM, autostart) and a main-window renderer-ready
  watchdog reloads the Settings page once if it never mounts (black-window
  self-heal, logged either way).
- **Never reuse `MUTED`/`PAUSED` across takes.** Both globals reset when a new
  recording starts (`show_recording` stores `PAUSED=false`).
- **Frontend state needs polling on mount.** `recorder.tsx` pulls state + uses a
  1s poll + listens `video_recorder_state`, so the toolbar reflects
  armed/recording/muted/paused even if it mounts after the event fired.

---

## Invoking from the frontend (api.ts)

The Rust commands (see `invoke_handler` in `lib.rs`) are wrapped in `src/api.ts`:
`videoRecordBegin`, `videoRecordStop`, `videoToggleMute`, `videoTogglePause`,
`videoRecordState`/`videoMuteState`/`videoPauseState` (state getters). Preview
urls for history come through the normal `getCapturePreview` path.

## How to verify it still works

1. `npm run tauri dev`, press `Ctrl+Alt+V`, draw a region.
2. Expect: dashed border + size pill, toolbar with a big circular **Rec**
   button, app window hidden.
3. Click Rec → timer ticks; Mute and Pause toggle; drag toolbar over the region
   (it must NOT appear in the footage — capture exclusion).
4. Stop (toolbar or tray) → MP4 appears in Settings history with a play badge
   and a real thumbnail; double-click opens the default player.
5. Check the debug log ends with `video: added to history ->` and no
   `WARNING ... timed out`.
6. `cargo test --manifest-path src-tauri/Cargo.toml` — currently 26/26 pass.

**Live reproduction is the only real test** — this feature needs an actual
display, GPU, and system audio to validate; don't trust a green compile alone.