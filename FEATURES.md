# SnapDrop — Feature Roadmap

Planned features beyond the current release. SnapDrop is a 100% local-first
Windows capture tool: screenshots never leave your device unless you drag
them somewhere yourself.

---

## Delayed capture ✅ Completed

Select the area first, then the shot fires after a countdown — for capturing
dropdown menus and tooltips that close when they lose focus.

- **Shipped**: hold **Shift while selecting** to arm it (same muscle memory as
  Ctrl flipping the editor). The duration comes from Settings → Capture →
  "Shift+select delay (seconds)" (3/5/10…; 0 = Shift does nothing).
- On release the overlay turns click-through and counts down (ghost outline
  + remaining seconds) so you can open the menu or hover the tooltip; the
  shot fires automatically. Esc cancels. Normal captures stay instant.

## Last area re-capture ✅ Completed

Re-capture the same region without re-drawing the box — the Icecream "Last
area" muscle memory.

- **Shipped**: press **Ctrl+Alt+4** (configurable in Settings → Capture →
  "Last Area Hotkey") and the selection overlay opens already positioned on
  the previously captured area, with corner handles.
  **Click the box to capture instantly**; drag inside to move it, drag an
  edge to resize it, drag elsewhere for a fresh selection (which becomes the
  new last area). Esc cancels. Every image capture records the new last
  area automatically.
- Composes with the other overlay modifiers: hold **Shift** while clicking to
  arm the delayed capture, **Ctrl** to flip the editor decision.

## Multi-file drag from the stack ✅ Completed

Drag **several captures into Discord in one gesture** instead of one at a
time.

- **Shipped**: in the history gallery, **Ctrl+click** (or Shift+click for a
  range) selects multiple captures — blue highlight + checkmark — and
  dragging any selected row/card shares the whole set as separate
  attachments. Esc or clicking empty canvas clears the selection.
- `dragdrop.rs` now builds a multi-path `CF_HDROP`; the drag image comes
  from the first file and move-drop cleanup deletes every dragged file when
  "keep file after drag" is off.
- Possible follow-up: multi-drag straight from the floating thumbnail stack.

## OCR — copy text from a screenshot ✅ Completed

Draw a region → the extracted text lands on your clipboard.

- **Shipped**: global hotkey **Ctrl+Shift+5** (configurable in Settings →
  Capture → "Text Capture Hotkey") captures a region and copies the
  recognized text to the clipboard — no file saved, no thumbnail.
  The annotation editor also has an **🔤 OCR** button that recognizes the
  current capture.
- Uses the built-in **offline** Windows OCR (`Windows.Media.Ocr`), so it
  stays 100% local-first like the rest of the app — no cloud, no API keys.
- Possible follow-ups: OCR search over history, an "OCR + copy image"
  combined action, and a language picker for machines with several
  installed OCR languages.

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
