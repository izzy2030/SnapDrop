# SnapDrop

**Windows screenshot capture & drag-to-drop utility** — capture a screen region and drag
the floating thumbnail straight into another application as a real file drop. No Explorer,
no save dialog, no searching for the file.

Built with **Tauri 2** (React + TypeScript frontend, Rust backend). Windows 10/11 only.

## Workflow

1. Press `Ctrl + Shift + 4` (configurable).
2. Drag a selection rectangle over the screen (Esc cancels). Hold **Ctrl** while
   dragging to skip annotations entirely and go straight to the thumbnail.
3. Otherwise, an annotation editor appears over the captured area: pen,
   highlighter, arrow, rectangle, undo, color picker. Press **Enter** to confirm
   (the annotated image replaces the original file) or **Esc** to skip annotations.
4. A floating thumbnail appears in the bottom-left corner of the monitor.
5. Grab the thumbnail and drop it into any app that accepts files — it receives the PNG
   exactly as if it were dragged from Explorer. (Press, then move; a plain click on the
   thumbnail does not start a drag.) The thumbnail dismisses itself once the drop succeeds
   (Settings → Behavior → *Hide thumbnail after successful drop*).

Thumbnail gestures (no buttons): **double-click** opens the image in your default viewer,
**Ctrl+click** reveals it in Explorer, **Esc** hides the thumbnail (the file stays).

Disable the editor step anytime in Settings → Capture → *Annotate before showing thumbnail*.

The screenshot is also copied to the clipboard (image + file), so plain `Ctrl+V` works too.

## Features

- Global hotkey (works while SnapDrop runs in the background), configurable with conflict detection
- Region capture across multiple monitors with correct physical-pixel coordinates (Per-Monitor v2 DPI)
- PNG saved automatically to `%USERPROFILE%\Pictures\SnapDrop` with collision-safe filenames
- Native Windows drag-and-drop (COM `IDataObject` + `IDropSource` + `DoDragDrop`, with a shell drag image). The drag runs on the app's main UI thread — the same thread pattern as the official windows-rs sample — so Chromium-based drop targets (WebView2 / Electron chat apps) accept the file. The grab/move threshold is detected via global mouse polling, so the drag starts even after the cursor leaves the small thumbnail window
- Floating always-on-top thumbnail: drag, copy, open, delete, close; older captures stack behind it
- System tray: Capture Region, Settings, Recent Captures, Open Screenshot Folder, Pause Hotkey, Exit
- Close-to-tray; optional start-with-Windows (default on)
- Lightweight recent-capture history (last 10 by default, persisted)
- Local-first: no network, no accounts, screenshots never leave the machine

## Development

```bash
npm install
npm run tauri dev        # debug run
npm run icon             # regenerate icons (scripts/gen-icon.mjs → tauri icon)
npm test                 # frontend typecheck + build
cd src-tauri && cargo test
cargo run --example capture_smoke   # smoke-test the capture pipeline on this machine
npm run tauri build      # release + NSIS installer in src-tauri/target/release/bundle
```

### Architecture

| Layer | Responsibility |
| --- | --- |
| Rust | Global hotkey, monitor enumeration, **native overlay** (layered Win32 window), per-monitor GDI `BitBlt` capture, PNG save, clipboard (CF_DIBV5 + CF_HDROP), native drag-and-drop, tray, settings/history persistence, autostart |
| React | Thumbnail UI (stack, drag gesture, actions, toasts), Settings UI (hotkey recorder, folder, thumbnail, behavior), History list |

Key modules in `src-tauri/src/`: `overlay.rs` (selection overlay), `capture.rs` (capture/stitch),
`dragdrop.rs` (COM drag source), `hotkey.rs`, `tray.rs`, `settings.rs`, `history.rs`.

## Manual test checklist (real-world workflow)

### Capture
- [ ] Single monitor: hotkey → overlay → selection → PNG appears as thumbnail
- [ ] Dual monitors: capture on the secondary display; selection may span monitors
- [ ] 100% / 125% / 150% / 200% scaling: selection matches the captured pixels exactly
- [ ] Small, large, and full-screen selections
- [ ] Esc cancels; click without dragging cancels
- [ ] Rapid successive captures produce unique filenames (`_1`, `_2` suffixes)

### Drag (the defining feature)
Drop the thumbnail into each of:
- [ ] File Explorer (copies the PNG)
- [ ] Browser upload fields (e.g. GitHub / Gmail / chat)
- [ ] Discord
- [ ] Paint / Photoshop
- [ ] Word / PowerPoint
- [ ] An AI coding application (Cursor / Claude / ChatGPT)

### Clipboard
- [ ] `Ctrl+V` pastes the image into a chat / editor
- [ ] Repeated captures replace the clipboard cleanly

### Lifecycle
- [ ] Starts in the tray, main window hidden; closing Settings hides to tray
- [ ] Tray → Recent Captures re-opens older captures as draggable thumbnails
- [ ] Monitor disconnect / reconnect and sleep / wake don't break the next capture

## Known limitations (v1)

- The mouse cursor is not included in captures (GDI screen DCs exclude it).
- DRM-protected / exclusive-fullscreen content may capture as black (standard GDI behavior).
- First WebView2 thumbnail show can be a few hundred ms; subsequent ones are instant.
- Hotkey conflicts with other apps are reported on startup and in Settings.

## Privacy

SnapDrop makes no network requests. Screenshots are stored only in the configured local folder.
