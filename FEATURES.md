# SnapDrop — Feature Roadmap

Planned features beyond the current release. SnapDrop is a 100% local-first
Windows capture tool: screenshots never leave your device unless you drag
them somewhere yourself.

---

## Delayed capture

Hotkey modifier or tray option for a **3/5-second timer**, for capturing
dropdown menus and tooltips that close when they lose focus.

- Trigger the normal selection overlay after the countdown.
- Countdown indicator on screen so you know when the shot fires.

## Multi-file drag from the stack

Drag **3 recent captures into Discord in one gesture** instead of one at a
time.

- The OLE drag in `dragdrop.rs` already builds a `CF_HDROP`; extending it to
  several files is a small change.
- Selection UI: expand the thumbnail stack (or use the history gallery) and
  drag the selected set.

## OCR — copy text from a screenshot

Draw a region → the extracted text lands on your clipboard.

- Uses the built-in **offline** Windows OCR (`Windows.Media.Ocr`), so it
  stays 100% local-first like the rest of the app — no cloud, no API keys.
- The single biggest utility add for a capture tool.
- Natural follow-ups once the base works: OCR search over history, and an
  "OCR + copy image" combined action.

## Last-capture hotkeys

Global shortcuts so you can re-share the previous capture without
recapturing:

- **Re-copy last shot** — image + file back onto the clipboard.
- **Open last shot** — default image viewer.
- **Re-drag last shot** — re-arms the OLE drag with the most recent file so
  the next drop target you click gets it.

## Editor: arrows, text labels, numbered steps, blur/pixelate

The annotation set people actually use for **bug reports and tutorials**:

- Arrows and text labels for callouts.
- Auto-incrementing numbered steps (1, 2, 3…) for walkthroughs.
- Blur/pixelate regions for hiding emails, tokens, and personal data.
- The editor already has strokes + undo; these are incremental tools on the
  same canvas.

## Screen recording → GIF/MP4

Region recording from the **same selection overlay** used for screenshots.

- Big differentiator; meaningful work.
- Needs: a capture loop over the selected region, an encoder pipeline
  (GIF for chat-friendly loops, MP4 for quality), a stop control (hotkey or
  tray), and size-friendly frame pacing.
