# SnapDrop system-freeze investigation

**Date:** 2026-09-17
**Status:** initial findings, not confirmed root cause

## What happened

A capture was taken with **Ctrl held during the selection drag**, which routes into
the annotation-editor path (`capture_flow::finish_capture` → `editor::show`). Right
after that, **every open application stopped responding**.

### What the freeze looks like (per user, 9/17)

- Mouse cursor **moves** freely.
- **Typing works** (keyboard input is delivered).
- **No open app reacts to clicks**: tab switching does nothing in Ripflow, Freebuff,
  or Antigravity; nothing clickable inside any of them.
- **The system itself is not frozen** — only the open apps are unresponsive.
- **Task Manager opens but is click-dead too** — even a brand-new process is caught in
  the same stall, so it is a shared window-message/presentation resource, not something
  local to the apps that happened to be open.
- The freeze did **not** lift on its own — it required a **sign-out / sign-back-in**.
- A **text select+copy in the terminal froze that terminal as well** (done while the
  freeze was already in progress). Any copy action needs the single-owner clipboard —
  see "Reading" for why that blocks.
- The freeze was preceded by normal operation ("before that screenshot the computer
  was working fine").

## OS evidence — a stall, not a crash

Checked on 9/17 after the incident:

| Check | Result |
|---|---|
| Reboot since 9/14 11:28 AM (last boot) | **No** — the freeze cleared without a power reset |
| Kernel-Power 41 / Event 6008 (unclean shutdown) | None today |
| System bugcheck (BSOD) | None at freeze time |
| LiveKernelEvent 117 / 141 (GPU TDR) via WER | **None** at freeze time — WER queue only shows re-uploads of older kernel reports (crash params identical to historical ones) |
| Display Event 4101 `amdwddmg`/`Display` | None |
| DWM restart / explorer crash | None |
| SnapDrop WER (AppCrash/AppHang) | None today |

Historical context (this machine, before today):
- Recurrent bugchecks, incl. memory-related stop codes (4e/18/1a/50/139), latest real
  BugCheck 9/6.
- Recurrent AMD GPU live-kernel 117/141 (VIDEO_TDR_TIMEOUT / VIDEO_ENGINE_TIMEOUT) —
  RX 580, driver 31.0.21925.1001.
- SnapDrop itself has a chronic crash/hang WER history: **12 AppCrash + 4 AppHang
  entries on 8/24–8/25** (none since).

## Code path involved

User presses Ctrl **while dragging** → overlay reports `sel.ctrl_held = true` →
`capture_flow::finish_capture` (`src-tauri/src/capture_flow.rs:294`) XORs the
setting (`show_editor_after_capture != sel.ctrl_held`) → editor path runs:

1. `capture::capture_region` (`src-tauri/src/capture.rs:24`) — for **each** monitor in
   the selection, a **synchronous full-monitor GDI readback on the app's main thread**:
   `BitBlt` (capture.rs:81) + `GetDIBits` (capture.rs:96) from a screen DC.
2. Full PNG re-encoded to base64 (capture_flow.rs:301-310) — the whole screenshot
   as one big string handed to the webview.
3. `editor::show` (`src-tauri/src/editor.rs:59`) on the **same** main thread:
   - resizes the editor window to cover the whole monitor (`set_size`, editor.rs:93),
   - `set_always_on_top(true)` (editor.rs:99),
   - `set_focus()` (editor.rs:100),
   - emits the full-res image to a **WebView2 (Chromium)** window.

## Reading of the symptoms

The fact that the pointer moves, typing works, the system responds, and yet **every
open app ignores clicks** points to a **window-message / presentation-level stall that
all GUI processes share**, specifically in a resource they all contend for — not a GPU
TDR (none logged; also the screen would flicker/reset) and not a kernel hang (system
would die).

The concrete suspects, in order:

1. **Full-desktop GDI readback (`BitBlt`) blocking the shared display-present/window
   path.** This call runs on the main thread and needs the driver/DWM to hand over the
   frame. On this RX 580 (prior TDR history) a stuck readback can block the shared WDDM
   presentation path every app's frame is waiting on. GDI-path stalls of this kind
   frequently log **nothing** (the D3D TDR watchdog does not cover them).
2. **The always-on-top, focus-stealing, fullscreen WebView2 editor window created in the
   same moment** (editor.rs:93-100). WebView2/Chromium spins up GPU/compositor processes;
   combined with the capture readback still in flight this can wedge the compositor path
   for other windows.
3. The heavy base64 handoff (capture_flow.rs:301) is a latency factor, not a global
   freeze factor by itself (everything runs on one thread — slow, but not system-wide).

A second candidate for the *click*-dead session and the one that explains the **terminal
copy freezes too**: the **synchronous clipboard write on the same thread**
(`clipboard::set_image_and_file`, clipboard.rs:54-78, called from capture_flow.rs:265-277
right after the readback). `OpenClipboard` → `SetClipboardData` → `CloseClipboard` all run
back-to-back on SnapDrop's main thread. If that thread stalls **while the clipboard is
open**, `CloseClipboard` never runs — and any other app that tries to copy blocks inside
its own `OpenClipboard` (a ~20 s retry loop that parks the calling app's UI thread). That
matches the report exactly: the user selected text in the terminal, triggered a copy, and
the terminal froze at that moment. The clipboard is a single global lock, so a capture
wedge converts a local stall into cross-process hangs.

GDI BitBlt is the reason the freeze can span *the whole session*: every window needs the
same underlying DWM/driver surface to repaint, and a synchronous blocked repaint path
shows up as "tabs won't switch, nothing clickable" in all apps.

**Post-incident evidence sharpens this** (see "Post-sign-in log evidence" below): the
frozen session's process **survived the freeze** — it logged a clean-exit marker when the
user signed out. A main thread stuck *inside* `BitBlt`/`GetDIBits` could not run the exit
handler, so SnapDrop was pumping (at least at the end of the session). That moves the wedge
to the **shared present path** rather than SnapDrop's own thread being stuck in GDI. It also
weakens the clipboard-lock theory as the cause of the frozen terminal copy: if SnapDrop's
thread was alive, its `OpenClipboard`→`CloseClipboard` completed, so the terminal's copy
freeze is better explained by the copy/selection forcing that terminal to *repaint* on the
stalled present path. This remains consistent with — but not proof of — the BitBlt trigger;
either way, never issuing a full-desktop GDI readback removes the stuck call.

## Questions still open

Answered (user follow-up, 9/17):

| Question | Answer |
|---|---|
| Did it end by itself? Duration? | **No** — cleared only by sign-out → sign-in |
| Task Manager / another app while frozen? | Opens, but **completely unresponsive** |
| Any copy/clipboard interaction? | **Yes** — selecting+copying terminal text froze that terminal too (see Reading) |
| What else was running? | An assistant session in a terminal (the same one that froze on copy); the conversation was lost in the sign-out |
| Single or multiple monitors? | **1** — `2560x1440@(0,0)`, DPI 1.15 (post-sign-in log) |
| Did SnapDrop itself survive the freeze? | **Yes** — exited with a clean marker at sign-out; see Reading |

Still open:

- Does it reproduce reliably with the same steps (hold Ctrl while selecting)? One incident so far.
- Installed app or dev build (`npm run tauri dev`)? The frozen terminal hints dev mode — unconfirmed.
- Any SnapDrop **video recording** active at the time?

## Post-sign-in log evidence (9/17, 23:24:48)

After sign-out → sign-in the app relaunched (autostart enabled) with a fresh session id
`4f5b2e296115f4b0`. Confirmed from `snapdrop-debug.log`:

- **Single monitor, healthy environment** — `startup context: monitors=1
  virtual=2560x1440@(0,0) scales=["1.15"] dwm=true autostart=true`. Cross-monitor
  stitching is not a factor.
- **The frozen session ended with a clean-exit marker** — `previous session: ended
  cleanly`, i.e. the rotated log's tail contained "session end (clean exit)", which is
  written by the `RunEvent::Exit` handler (lib.rs:376). The frozen process therefore
  handled the end-session event at sign-out instead of being killed.
- The new session's IPC all answers `ms=0` — nothing remained wedged once the session was
  torn down.

## Fix attempt (9/17)

Implemented: **image capture now uses Windows Graphics Capture (WGC) instead of the
synchronous full-desktop GDI readback** — the Rank-1 fix — reusing the vendored
`windows-capture` crate that already drives video recording.

- `capture::capture_region` stitches per-monitor frames from a one-shot WGC session
  (`capture::capture_monitor_wgc`); the old GDI `capture_monitor` remains only for the
  `capture_smoke` example.
- WGC runs on its own thread (`ShotHandler::start_free_threaded`, the same pattern video
  recording uses), so no `BitBlt`/`GetDIBits` executes on the app/event thread.
- Bounded so a stuck GPU degrades instead of wedging: timeout of 8 s awaiting the first
  frame (first frame arrives on session start even on a static screen) → capture fails to
  an error toast; the WGC thread is joined with a 5 s bound and **leaked rather than
  blocking the flow** if it is truly wedged (a `ponytail:` comment in `capture.rs` marks
  that ceiling — re-check if WGC threads ever accumulate).
- **No GDI fallback by design** — re-adding it would re-add the freeze path. If WGC init
  fails, the capture degrades to a toast.
- Settings match the old output: `ColorFormat::Bgra8`, cursor excluded
  (`CursorCaptureSettings::WithoutCursor`), no border; WGC sets alpha opaque (255) where
  GDI used 0, which downstream encode/clipboard already overwrite.

Deliberately **not** done in this pass — still the ranked follow-ups: move the PNG
encode/clipboard write off the flow thread (Rank 2), defer the editor focus/topmost dance
(Rank 3). The strongest global-freeze suspect (the GDI readback) is removed.

Verification: `cargo check --all-targets` passes (only the pre-existing vendored-crate
dead-code warning). **Still needs a manual capture smoke test on the RX 580** before this
can be considered validated.

## Recommended fixes (ranked)

1. **✅ DONE (9/17)** — Switch image capture from GDI `BitBlt` to Windows Graphics
   Capture (WGC). The codebase already vendors `windows-capture` and uses WGC for video
   recording (`graphics_capture_api.rs`). WGC is asynchronous and does not hold up the shared
   present path the way a synchronous full-desktop BitBlt does. (Implementations in
   `capture.rs`; see "Fix attempt" above.) → removes the primary global-freeze suspect.
2. **Run capture + encode off the main thread** with a timeout, and only then show the
   editor — keeps SnapDrop's own UI alive and lets it error out instead of stalling.
3. **Defer the focus-steal/always-on-top dance** (editor.rs:99-100) until the image is
   rendered, and leave focus alone when the editor is just annotating.
4. If a quick mitigation is needed before the code change: capture without Ctrl, or
   disable the editor path in settings, until WGC migration lands.

## Files referenced

- `src-tauri/src/capture.rs` — **WGC one-shot image capture** (now `capture_region` →
  `capture_monitor_wgc`, `ShotHandler`); GDI `BitBlt`/`GetDIBits` path kept only for the
  `capture_smoke` example (was lines 24, 61, 81, 96)
- `src-tauri/src/capture_flow.rs` — flow + Ctrl/XOR + editor branch (lines 210, 294-311)
- `src-tauri/src/clipboard.rs` — synchronous Open/Set/CloseClipboard on the capture thread
  (lines 54-78), a single global lock that blocks any other app's copy while held
- `src-tauri/src/editor.rs` — editor lifecycle, fullscreen topmost webview (lines 59-109);
  Rank-3 follow-up (defer focus/topmost) still pending
- `src-tauri/vendor/windows-capture/src/graphics_capture_api.rs` — WGC pipeline; **now used
  for both image and video capture**