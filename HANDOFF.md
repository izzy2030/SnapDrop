# SnapDrop — Handoff Document

**Version:** 1.0.0
**Platform:** Windows 10/11 only
**Stack:** Tauri 2 (Rust backend + React/TypeScript frontend)
**Last updated:** August 24, 2026

---

## What SnapDrop Does

SnapDrop is a lightweight Windows screenshot utility built around a single workflow:

> **Keyboard shortcut → Select region → Annotate (optional) → Floating thumbnail → Drag into another app**

The goal is to eliminate every unnecessary step between capturing a screenshot and giving it to another application, particularly AI coding agents. No file browser, no save dialog, no searching for the file.

---

## Core Features (all implemented)

1. **Global hotkey capture** (`Ctrl+Shift+4`, configurable) — works while SnapDrop runs in the background
2. **Region capture** across multiple monitors with correct physical-pixel coordinates (Per-Monitor v2 DPI awareness)
3. **Annotation editor** — pauses after capture with a drawing toolbar (pen, highlighter, arrow, rectangle, undo) before showing the thumbnail. Enter confirms, Esc skips.
4. **Floating thumbnail** — transparent, always-on-top, positioned in the bottom-left corner of the capture monitor, inside the work area (clear of the taskbar)
5. **Native drag-and-drop** — the defining feature. COM `IDataObject` (CF_HDROP) + `IDropSource` + `DoDragDrop` with a shell drag image. External apps receive the screenshot as a real PNG file drop. Runs on the main UI thread (Chromium/WebView2 apps require this).
6. **Clipboard** — screenshots are copied to clipboard (CF_DIBV5 + CF_HDROP) immediately after capture, so `Ctrl+V` works too
7. **Thumbnail gestures** (no buttons):
   - **Double-click** → opens in default image viewer
   - **Ctrl+click** → reveals in Explorer
   - **Esc** → hides the thumbnail (file stays on disk)
   - **Press + drag** → native file drag-and-drop
8. **Auto-dismiss after drop** — thumbnail vanishes when a drag lands successfully (configurable)
9. **Capture history** — persisted last-10 stack, accessible from tray
10. **System tray** — Capture Region, Settings, Recent Captures, Open Screenshot Folder, Pause Hotkey, Exit
11. **Close-to-tray** — closing the main window minimizes to tray
12. **Settings** — hotkey, folder, thumbnail size/position/duration, autostart, editor toggle
13. **Windows startup** — optional, default on

---

## Architecture

```
┌──────────────────────────────────────┐
│         React/TypeScript UI          │
│                                      │
│  Main Window (Settings)              │
│  Thumbnail Window (transparent,      │
│    always-on-top, gesture-driven)    │
│  Editor Window (annotation canvas,   │
│    drawing tools, confirm/cancel)    │
└──────────────────┬───────────────────┘
                   │ Tauri IPC + Events
┌──────────────────▼───────────────────┐
│           Rust Backend               │
│                                      │
│  overlay.rs     — Win32 selection    │
│  capture.rs     — GDI BitBlt        │
│  clipboard.rs   — CF_DIBV5/CF_HDROP │
│  dragdrop.rs    — COM DoDragDrop     │
│  editor.rs      — editor lifecycle   │
│  thumbnail.rs   — thumbnail window   │
│  hotkey.rs      — global shortcuts   │
│  tray.rs        — system tray        │
│  settings.rs    — JSON persistence   │
│  history.rs     — recent captures    │
│  capture_flow.rs — orchestrator      │
│  filename.rs    — collision-safe PNG │
│  monitors.rs    — multi-monitor/DPI  │
│  dpi.rs         — Per-Monitor v2     │
│  notifier.rs    — toast errors       │
└──────────────────┬───────────────────┘
                   │
              Windows APIs
```

---

## Key Technical Decisions & Fixes

### 1. Capture: Per-Monitor DPI Awareness
Set explicitly at process start via `SetProcessDpiAwarenessContext(PROCESS_PER_MONITOR_DPI_AWARE)`. All coordinates are physical pixels. The overlay spans the virtual screen and converts coordinates to physical pixels using per-monitor DPI values.

### 2. Capture: Native Win32 Overlay
A single layered window (WS_EX_LAYERED + WS_EX_TRANSPARENT + WS_EX_TOPMOST) spans the virtual screen. Crosshair cursor, selection rectangle with border, live dimension readout, Esc/short-click cancel. All in Rust, no webview involvement.

### 3. Drag-and-Drop: Main Thread Requirement
**The critical fix.** `DoDragDrop` was initially run on a dedicated worker thread. Chromium-based drop targets (WebView2/Electron apps like this chat client) reject OLE drags whose source thread isn't the main UI thread. Fix: run `DoDragDrop` inline on Tauri's main thread (where commands execute), with the gesture polling (6px movement threshold) on a worker thread.

### 4. Drag-and-Drop: Windows 0.62 API Changes
The `windows` crate v0.62 split input APIs into `UI::Input::KeyboardAndMouse` and OLE types into `System::Ole`. The `#[implement]` macro moved to `windows-core` and generates `*_Impl` wrapper structs. `SHDoDragDrop` was removed from the binding; linked directly against shell32 via `#[link(name = "shell32")]`.

### 5. Editor: Global Hotkey Deadlock (Esc)
The global Esc hotkey (used to dismiss the thumbnail) intercepted Esc before the editor webview could see it. The editor couldn't receive the key at all. Fix: the Esc hotkey handler checks whether the editor is visible — if so, it emits `editor-cancelled`; otherwise it hides the thumbnail.

### 6. Editor: Enter Hotkey Deadlock
An attempt to register a global Enter hotkey while the editor was open caused a deadlock — `editor::show` runs on the main thread (inside the capture flow), and the plugin's `register` internally does `run_on_main_thread` + blocks waiting. The main thread deadlocked on itself. Fix: removed the Enter global hotkey entirely; the editor webview handles Enter via its own keydown (the editor window is focusable).

### 7. Editor: Event-Listener Registration Race
The editor's confirm/cancel listeners were registered inside async tasks at startup. If the first capture confirmed before they finished, the event was silently dropped. Fix: listeners are now registered synchronously at startup.

### 8. Thumbnail: Focus-less by Design
The thumbnail window uses `focus: false` so it never steals keyboard focus from other apps. This means it cannot receive keyboard events directly — Esc is handled via the global hotkey instead.

---

## Capture Flow (Happy Path)

```
Ctrl+Shift+4
    ↓
hotkey::trigger_capture
    ↓
capture_flow::run (main thread)
    ↓
hide thumbnail + show overlay
    ↓
user selects region
    ↓
capture_region (GDI BitBlt, per-monitor DPI)
    ↓
encode PNG + save to Pictures\SnapDrop
    ↓
copy to clipboard (CF_DIBV5 + CF_HDROP)
    ↓
add to history
    ↓
if show_editor_after_capture:
    encode full PNG → editor::show → show editor window
    ↓
    user annotates → Enter confirms
    ↓
    editor-confirmed event → on_confirmed
    ↓
    decode annotated PNG → save over original → re-copy to clipboard
    ↓
    finish → hide editor → thumbnail::show_capture_for_at
else:
    thumbnail::show_capture (with preview)
```

---

## File Structure

```
SnapDrop/
├── src/
│   ├── main.tsx              — Settings window entry
│   ├── thumbnail.tsx         — Thumbnail window entry
│   ├── editor.tsx            — Editor window entry
│   ├── ThumbnailApp.tsx      — Thumbnail React component
│   ├── EditorApp (editor.tsx)— Editor React component (drawing tools)
│   ├── SettingsApp.tsx       — Settings React component
│   ├── api.ts                — Tauri IPC wrappers
│   └── styles.css            — All styles (dark theme)
├── src-tauri/
│   ├── src/
│   │   ├── main.rs           — Entry point
│   │   ├── lib.rs            — Tauri builder, plugin registration
│   │   ├── overlay.rs        — Win32 capture overlay
│   │   ├── capture.rs        — GDI BitBlt capture
│   │   ├── capture_flow.rs   — Orchestrator (capture → save → show)
│   │   ├── clipboard.rs      — Windows clipboard (CF_DIBV5 + CF_HDROP)
│   │   ├── dragdrop.rs       — COM DoDragDrop (IDataObject + IDropSource)
│   │   ├── editor.rs         — Editor lifecycle (show/confirm/cancel)
│   │   ├── thumbnail.rs      — Thumbnail window management
│   │   ├── hotkey.rs         — Global shortcuts (capture + Esc)
│   │   ├── tray.rs           — System tray menu
│   │   ├── settings.rs       — JSON settings persistence
│   │   ├── history.rs        — Recent captures (last 10)
│   │   ├── filename.rs       — Collision-safe filenames + PNG encode
│   │   ├── monitors.rs       — Multi-monitor enumeration + DPI
│   │   ├── dpi.rs            — Per-Monitor v2 DPI awareness
│   │   └── notifier.rs       — Toast notifications
│   ├── Cargo.toml            — Rust dependencies
│   ├── tauri.conf.json       — Tauri config (3 windows)
│   └── capabilities/default.json
├── index.html                — Main window HTML
├── thumbnail.html            — Thumbnail window HTML
├── editor.html               — Editor window HTML
├── vite.config.ts            — Vite multi-page build
├── package.json              — Node dependencies + scripts
├── scripts/gen-icon.mjs      — App icon generator
└── README.md                 — User-facing docs + test checklist
```

---

## Build Commands

```bash
npm install                  # Install Node dependencies
npm run tauri dev            # Debug run (hot reload)
npm run build                # Frontend only (tsc + vite)
cargo test                   # Rust unit tests (9 tests)
cargo check                  # Rust type check
npm run tauri build          # Release + NSIS installer
```

Installer output: `src-tauri/target/release/bundle/nsis/SnapDrop_1.0.0_x64-setup.exe`

---

## Known Limitations

- **DRM-protected content** (YouTube video frames) captures as black — standard GDI behavior, not fixable without Desktop Duplication API
- **Cursor not captured** — GDI screen DCs exclude the mouse cursor
- **First editor load** can be a few hundred ms; subsequent ones are instant
- **Esc hotkey** is registered at startup and acts only while the thumbnail or editor is visible — it cannot conflict with other apps when those windows are hidden, but there is a theoretical conflict if another app claims bare Esc globally while the thumbnail is up

---

## What Was Built in This Session

1. **Full Tauri 2 + React/TypeScript project scaffold** — from scratch, no prior repo
2. **Complete capture engine** — per-monitor GDI BitBlt, overlay, DPI handling, PNG save, collision-safe filenames
3. **Native COM drag-and-drop** — the defining feature, with shell drag image and main-thread fix
4. **Annotation editor** — full-screen canvas with pen/highlighter/arrow/rectangle/undo, Enter/Esc confirm/cancel
5. **Thumbnail window** — transparent, always-on-top, gesture-based (no buttons), auto-dismiss after drop
6. **System tray** — full menu with Recent Captures, settings, pause hotkey
7. **Settings UI** — hotkey recorder, folder picker, thumbnail controls, editor toggle, behavior options
8. **App icon** — generated via canvas script
9. **NSIS installer** — built and rebuilt multiple times through iterations
10. **Multiple rounds of bug fixing** — main-thread drag, Esc deadlock, Enter deadlock, event-listener race, Windows 0.62 API drift

---

## Testing Checklist

### Capture
- [ ] Single monitor
- [ ] Dual monitors
- [ ] 100% / 125% / 150% / 200% scaling
- [ ] Small, large, full-screen selections
- [ ] Esc cancels; click without dragging cancels
- [ ] Rapid captures produce unique filenames

### Drag
- [ ] File Explorer
- [ ] Browser upload fields
- [ ] Discord
- [ ] Chat apps (WebView2/Electron)
- [ ] Image editors
- [ ] Office apps

### Editor
- [ ] Pen draws correctly
- [ ] Highlighter is semi-transparent
- [ ] Arrow tool works
- [ ] Rectangle tool works
- [ ] Undo removes last stroke
- [ ] Enter confirms → annotated file replaces original
- [ ] Esc cancels → original file untouched
- [ ] Toolbar buttons work (✓ and ✕)

### Thumbnail
- [ ] Double-click opens in default viewer
- [ ] Ctrl+click reveals in Explorer
- [ ] Esc hides the thumbnail
- [ ] Press+drag starts native file drag
- [ ] Thumbnail dismisses after successful drop

### Lifecycle
- [ ] Starts in tray, main window hidden
- [ ] Close-to-tray works
- [ ] Capture hotkey works from background
- [ ] Settings persist across restarts
- [ ] Windows startup works

---

## Product Principle

> **Never make the user do a step that the operating system can do automatically.**

The ideal interaction is: **Capture → Grab → Drop.**

Everything else stays out of the way.
