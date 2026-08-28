import { useCallback, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { emit, listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

type Tool = "pen" | "highlighter" | "arrow" | "rect" | "text" | "number" | "blur";

interface Stroke {
  tool: Tool;
  color: string;
  points: { x: number; y: number }[];
  /** Text label content (tool === "text"). */
  text?: string;
  /** Step number (tool === "number"); count-based so undo reuses the number. */
  n?: number;
}

const PALETTE = ["#ff4d4f", "#ffd43b", "#4dabf7", "#69db7c", "#ffffff", "#000000"];
const TOOL_WIDTH: Record<Tool, number> = {
  pen: 3,
  highlighter: 22,
  arrow: 3,
  rect: 3,
  text: 0,
  number: 0,
  blur: 0,
};
const TEXT_SIZE = 28; // image px
const NUMBER_RADIUS = 15; // image px

// Keyboard shortcuts: 1-7 pick tools while annotating.
const TOOL_KEYS: Record<string, Tool> = {
  "1": "pen",
  "2": "highlighter",
  "3": "arrow",
  "4": "rect",
  "5": "text",
  "6": "number",
  "7": "blur",
};

const TOOLS: { id: Tool; label: string; glyph: string }[] = [
  { id: "pen", label: "Pen", glyph: "✏️" },
  { id: "highlighter", label: "Highlighter", glyph: "🖍️" },
  { id: "arrow", label: "Arrow", glyph: "➜" },
  { id: "rect", label: "Rectangle", glyph: "▭" },
  { id: "text", label: "Text label", glyph: "T" },
  { id: "number", label: "Numbered step", glyph: "①" },
  { id: "blur", label: "Pixelate region", glyph: "▒" },
];

function EditorApp() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const [tool, setTool] = useState<Tool>("pen");
  const [color, setColor] = useState(PALETTE[0]);
  const [strokes, setStrokes] = useState<Stroke[]>([]);
  const strokesRef = useRef<Stroke[]>([]);
  const [fit, setFit] = useState({ w: 320, h: 200, scale: 1 });
  const [ready, setReady] = useState(false);
  const [ocrBusy, setOcrBusy] = useState(false);
  const [ocrMsg, setOcrMsg] = useState<string | null>(null);
  const ocrTimerRef = useRef<number | null>(null);
  // In-progress text label: image point + on-screen position + typed value.
  const [textEdit, setTextEdit] = useState<{
    img: { x: number; y: number };
    screen: { x: number; y: number };
    value: string;
  } | null>(null);
  const textEditRef = useRef(textEdit);
  textEditRef.current = textEdit;
  // Where the current pointer press began (click-vs-drag for text placement).
  const pressStartRef = useRef<{ x: number; y: number } | null>(null);

  const baseImageRef = useRef<HTMLImageElement | null>(null);
  const activeRef = useRef<Map<number, Stroke>>(new Map());

  strokesRef.current = strokes;

  const redraw = useCallback(() => {
    const canvas = canvasRef.current;
    const base = baseImageRef.current;
    if (!canvas || !base) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(base, 0, 0, canvas.width, canvas.height);
    drawStrokes(ctx, strokesRef.current, canvas.width, canvas.height, base);
  }, []);

  // Redraw whenever the committed strokes change. Click-placed annotations
  // (text labels, step numbers) bypass commitStroke's live drawing — this is
  // the safety net; commitPlaced below repaints synchronously.
  useEffect(() => {
    const id = requestAnimationFrame(redraw);
    return () => cancelAnimationFrame(id);
  }, [strokes, redraw]);

  // Load the captured image: pull it via IPC on mount (reliable even if the
  // webview wasn't ready when the editor event was emitted), and also
  // listen for the editor-scoped event as a backup.
  const loadImage = useCallback((src: string) => {
    const img = new Image();
    img.onload = () => {
      baseImageRef.current = img;
      const canvas = canvasRef.current;
      const wrap = wrapRef.current;
      if (!canvas || !wrap) return;
      canvas.width = img.naturalWidth;
      canvas.height = img.naturalHeight;
      const availW = wrap.clientWidth - 96;
      const availH = wrap.clientHeight - 56;
      const scale = Math.min(availW / img.naturalWidth, availH / img.naturalHeight, 1);
      setFit({ w: img.naturalWidth, h: img.naturalHeight, scale });
      setStrokes([]);
      setReady(true);
      requestAnimationFrame(() => {
        const ctx = canvas.getContext("2d");
        if (ctx) ctx.drawImage(img, 0, 0);
      });
    };
    img.src = src;
  }, []);

  useEffect(() => {
    void invoke<{ full: string; width: number; height: number } | null>(
      "get_pending_editor_image",
    ).then((p) => {
      if (p && p.full) loadImage(p.full);
    });
    const un = listen<{ full: string; width: number; height: number }>("editor-captured", (e) => {
      loadImage(e.payload.full);
    });
    return () => {
      un.then((f) => f());
    };
  }, [loadImage]);

  // Make sure the editor webview has keyboard focus so Enter/Esc/Ctrl+Z
  // reach the page (the window is focusable, but the inner webview may not be).
  useEffect(() => {
    window.focus();
    const t = window.setTimeout(() => window.focus(), 300);
    return () => window.clearTimeout(t);
  }, []);

  // Keyboard: Enter confirms, Esc cancels, Ctrl+Z undoes. Enter is handled by
  // the webview (the editor window has focus). Esc is consumed by SnapDrop's
  // global Esc hotkey before the webview sees it, so it arrives via the
  // "editor-cancelled" event from Rust instead.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // While a text label is being typed, keys belong to the input — Enter
      // commits the label, not the whole edit.
      if (textEditRef.current) return;
      const keyed = TOOL_KEYS[e.key];
      if (keyed) {
        e.preventDefault();
        setTool(keyed);
        void invoke("debug_log", { msg: `editor: tool=${keyed}` }).catch(() => {});
        return;
      }
      if (e.key === "Enter") {
        e.preventDefault();
        void confirmEdit();
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "z") {
        e.preventDefault();
        undo();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Esc is consumed by the global hotkey, which emits "editor-esc": close an
  // in-progress text input if one is open, otherwise cancel the whole edit.
  useEffect(() => {
    const un = listen("editor-esc", () => {
      if (textEditRef.current) {
        textEditRef.current = null;
        setTextEdit(null);
      } else {
        void emit("editor-cancelled", {});
      }
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  const toImgPoint = (e: React.PointerEvent): { x: number; y: number } => {
    const canvas = canvasRef.current!;
    const rect = canvas.getBoundingClientRect();
    const sx = canvas.width / rect.width;
    const sy = canvas.height / rect.height;
    return { x: (e.clientX - rect.left) * sx, y: (e.clientY - rect.top) * sy };
  };

  const onPointerDown = (e: React.PointerEvent) => {
    if (!ready) return;
    // Clicking the canvas while a label is being typed commits the label and
    // swallows the press so no stray stroke starts underneath the input.
    if (textEditRef.current) {
      commitText();
      return;
    }
    e.preventDefault();
    canvasRef.current?.setPointerCapture(e.pointerId);
    const p = toImgPoint(e);
    pressStartRef.current = p;
    if (tool === "text") return; // placed on pointer-up (a click)
    if (tool === "number") return; // placed on pointer-up
    const stroke: Stroke = { tool, color, points: [p] };
    activeRef.current.set(e.pointerId, stroke);
  };

  const onPointerMove = (e: React.PointerEvent) => {
    if (!activeRef.current.has(e.pointerId)) return;
    const stroke = activeRef.current.get(e.pointerId)!;
    stroke.points.push(toImgPoint(e));
    commitStroke(stroke);
  };

  const onPointerUp = (e: React.PointerEvent) => {
    const p = toImgPoint(e);
    if (tool === "text") {
      const start = pressStartRef.current;
      pressStartRef.current = null;
      // Only a click (not a drag) opens the label input.
      if (start && Math.hypot(p.x - start.x, p.y - start.y) < 10) {
        setTextEdit({
          img: p,
          screen: { x: p.x * fit.scale, y: p.y * fit.scale },
          value: "",
        });
      }
      return;
    }
    if (tool === "number") {
      pressStartRef.current = null;
      // Count-based numbering: undoing a badge and placing a new one reuses
      // the same number.
      const n = strokesRef.current.filter((s) => s.tool === "number").length + 1;
      commitPlaced({ tool: "number", color, points: [p], n });
      return;
    }
    const stroke = activeRef.current.get(e.pointerId);
    if (!stroke) return;
    activeRef.current.delete(e.pointerId);
    pressStartRef.current = null;
    if (stroke.points.length > 1) {
      setStrokes((s) => [...s, stroke]);
    } else {
      commitStroke(stroke);
    }
  };

  // Commit a click-placed annotation (text label / step number).
  // Deterministic on purpose: update the ref + state and repaint the canvas
  // synchronously. The confirm path exports the canvas directly, so a redraw
  // that only runs later via effect + requestAnimationFrame can be swallowed
  // (canceled cleanup, throttled/occluded webview) — the annotation would
  // then be missing from the saved PNG. Sync repaint closes that race.
  const commitPlaced = (stroke: Stroke) => {
    const next = [...strokesRef.current, stroke];
    strokesRef.current = next;
    setStrokes(next);
    redraw();
  };

  // Commit the in-progress text label (Enter, blur, or clicking elsewhere).
  const commitText = () => {
    const te = textEditRef.current;
    textEditRef.current = null;
    setTextEdit(null);
    if (!te) return;
    const value = te.value.trim();
    if (!value) return;
    void invoke("debug_log", { msg: `editor: text committed "${value}"` }).catch(() => {});
    commitPlaced({ tool: "text", color, points: [te.img], text: value });
  };

  const commitStroke = (stroke: Stroke) => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const base = baseImageRef.current;
    if (base) ctx.drawImage(base, 0, 0, canvas.width, canvas.height);
    drawStrokes(ctx, [...strokesRef.current, stroke], canvas.width, canvas.height, base);
  };

  const undo = () => {
    setStrokes((s) => {
      const next = s.slice(0, -1);
      strokesRef.current = next;
      requestAnimationFrame(redraw);
      return next;
    });
  };

  // OCR: recognize text in the capture (offline Windows OCR) and copy it to
  // the clipboard. Feedback via a transient chip since the editor has no
  // toast system of its own.
  const runOcr = async () => {
    if (ocrBusy || !ready) return;
    setOcrBusy(true);
    setOcrMsg("Recognizing text…");
    const done = (msg: string) => {
      setOcrBusy(false);
      setOcrMsg(msg);
      if (ocrTimerRef.current) window.clearTimeout(ocrTimerRef.current);
      ocrTimerRef.current = window.setTimeout(() => setOcrMsg(null), 4000);
    };
    try {
      const text = ((await invoke<string>("ocr_pending_editor_image")) ?? "").trim();
      if (!text) {
        done("No text found");
        return;
      }
      const chars = text.length;
      await invoke("copy_text", { text });
      done(`Copied ${chars} characters to clipboard`);
    } catch (e) {
      done(`OCR failed: ${String(e)}`);
    }
  };

  const confirmingRef = useRef(false);

  const confirmEdit = useCallback(async () => {
    if (confirmingRef.current) return; // hotkey + webview may both fire
    confirmingRef.current = true;
    const canvas = canvasRef.current;
    if (!canvas) {
      confirmingRef.current = false;
      return;
    }
    // Final synchronous repaint straight from state, so the export can never
    // miss a committed annotation whose redraw frame got swallowed.
    redraw();
    const summary = strokesRef.current
      .map((s) =>
        s.tool === "text"
          ? `text@${Math.round(s.points[0].x)},${Math.round(s.points[0].y)}`
          : s.tool === "number"
            ? `num${s.n}@${Math.round(s.points[0].x)},${Math.round(s.points[0].y)}`
            : s.tool,
      )
      .join(" ");
    void invoke("debug_log", { msg: `editor: confirm strokes: ${summary}` }).catch(() => {});
    const blob = await new Promise<Blob | null>((res) => canvas.toBlob(res, "image/png"));
    confirmingRef.current = false;
    if (!blob) return;
    const reader = new FileReader();
    reader.onload = () => {
      void emit("editor-confirmed", { annotated: reader.result as string });
    };
    reader.readAsDataURL(blob);
  }, [redraw]);

  return (
    <div className="editor-root" ref={wrapRef}>
      <div className="editor-toolbar" role="toolbar" aria-label="Annotation tools">
        {TOOLS.map((t) => (
          <button
            key={t.id}
            type="button"
            className={`tool-btn${tool === t.id ? " active" : ""}`}
            title={t.label}
            onClick={() => {
              setTool(t.id);
              void invoke("debug_log", { msg: `editor: tool=${t.id}` }).catch(() => {});
            }}
          >
            <span className="tool-glyph">{t.glyph}</span>
          </button>
        ))}
        <div className="toolbar-sep" />
        <button type="button" className="tool-btn" title="Undo (Ctrl+Z)" onClick={undo} disabled={strokes.length === 0}>
          <span className="tool-glyph">↩</span>
        </button>
        <button type="button" className="tool-btn" title="Clear all" onClick={() => { setStrokes([]); strokesRef.current = []; requestAnimationFrame(redraw); }}>
          <span className="tool-glyph">🗑</span>
        </button>
        <div className="toolbar-sep" />
        <button
          type="button"
          className={`tool-btn${ocrBusy ? " active" : ""}`}
          title="Copy text from image (OCR)"
          onClick={() => void runOcr()}
          disabled={!ready || ocrBusy}
        >
          <span className="tool-glyph">🔤</span>
        </button>
        <div className="toolbar-sep" />
        <div className="color-row">
          {PALETTE.map((c) => (
            <button
              key={c}
              type="button"
              className={`swatch${color === c ? " active" : ""}`}
              style={{ background: c }}
              title={c}
              onClick={() => setColor(c)}
            />
          ))}
        </div>
        <div className="toolbar-sep" />
        <button type="button" className="tool-btn confirm" title="Confirm (Enter)" onClick={() => void confirmEdit()}>
          <span className="tool-glyph">✓</span>
        </button>
        <button type="button" className="tool-btn cancel" title="Skip annotation (Esc)" onClick={() => void emit("editor-cancelled", {})}>
          <span className="tool-glyph">✕</span>
        </button>
      </div>

      {ocrMsg && <div className="editor-ocr-chip">{ocrMsg}</div>}

      <div className="editor-canvas-wrap">
        <div
          className="editor-canvas-holder"
          style={{ position: "relative", width: fit.w * fit.scale, height: fit.h * fit.scale }}
        >
          <canvas
            ref={canvasRef}
            className="editor-canvas"
            style={{
              width: "100%",
              height: "100%",
              touchAction: "none",
            }}
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
            onPointerCancel={onPointerUp}
          />
          {textEdit && (
            <input
              className="editor-text-input"
              style={{
                left: textEdit.screen.x,
                top: textEdit.screen.y,
                fontSize: Math.max(12, TEXT_SIZE * fit.scale),
                color,
              }}
              value={textEdit.value}
              placeholder="Type a label…"
              autoFocus
              onChange={(e) => setTextEdit({ ...textEdit, value: e.target.value })}
              onKeyDown={(e) => {
                // Keep editor-level shortcuts out of the input.
                e.stopPropagation();
                if (e.key === "Enter") {
                  e.preventDefault();
                  commitText();
                }
              }}
              onBlur={() => commitText()}
            />
          )}
        </div>
      </div>

      <div className="editor-hint">
        <span>Enter <b>✓</b> confirm</span>
        <span>Esc <b>✕</b> skip annotations</span>
        <span>Ctrl+Z undo</span>
        <span><b>1-7</b> tools</span>
      </div>
    </div>
  );
}

function drawStrokes(
  ctx: CanvasRenderingContext2D,
  strokes: Stroke[],
  w: number,
  h: number,
  base: HTMLImageElement | null,
) {
  const scale = Math.min(
    w / (ctx.canvas.width || w) || 1,
    h / (ctx.canvas.height || h) || 1,
  );
  for (const s of strokes) {
    ctx.save();
    ctx.lineJoin = "round";
    ctx.lineCap = "round";
    ctx.strokeStyle = s.color;
    ctx.fillStyle = s.color;
    ctx.globalAlpha = s.tool === "highlighter" ? 0.45 : 1;
    ctx.lineWidth = (TOOL_WIDTH[s.tool] || 3) * scale;
    const pts = s.points;

    // Text label: single point + string; white outline keeps colored text
    // readable on dark backgrounds (terminals, dark UIs) — same halo the
    // numbered-step badges use.
    if (s.tool === "text" && s.text) {
      const p = pts[0];
      const fs = TEXT_SIZE * scale;
      ctx.font = `600 ${fs}px "Segoe UI", system-ui, sans-serif`;
      ctx.textBaseline = "top";
      ctx.lineWidth = Math.max(2, fs / 7);
      ctx.strokeStyle = "rgba(255,255,255,0.95)";
      ctx.strokeText(s.text, p.x, p.y);
      ctx.fillStyle = s.color;
      ctx.fillText(s.text, p.x, p.y);
      ctx.restore();
      continue;
    }

    // Numbered step: colored badge with a white halo + white number.
    if (s.tool === "number" && s.n) {
      const p = pts[0];
      const r = NUMBER_RADIUS * scale;
      ctx.globalAlpha = 1;
      ctx.beginPath();
      ctx.arc(p.x, p.y, r + 2.5 * scale, 0, Math.PI * 2);
      ctx.fillStyle = "rgba(255,255,255,0.92)";
      ctx.fill();
      ctx.beginPath();
      ctx.arc(p.x, p.y, r, 0, Math.PI * 2);
      ctx.fillStyle = s.color;
      ctx.fill();
      ctx.fillStyle = "#fff";
      ctx.font = `700 ${Math.round(r * 1.25)}px "Segoe UI", system-ui, sans-serif`;
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillText(String(s.n), p.x, p.y + r * 0.06);
      ctx.restore();
      continue;
    }

    if (pts.length < 2) {
      ctx.restore();
      continue;
    }

    if (s.tool === "blur") {
      // Pixelate the region of the BASE screenshot (never the annotations —
      // redrawing always samples the original pixels, so undo stays clean).
      const a = pts[0];
      const b = pts[pts.length - 1];
      const x = Math.min(a.x, b.x);
      const y = Math.min(a.y, b.y);
      const rw = Math.abs(b.x - a.x);
      const rh = Math.abs(b.y - a.y);
      if (rw >= 4 && rh >= 4 && base) {
        const block = Math.max(10, Math.round(Math.max(w, h) / 80));
        const tw = Math.max(1, Math.floor(rw / block));
        const th = Math.max(1, Math.floor(rh / block));
        const tmp = document.createElement("canvas");
        tmp.width = tw;
        tmp.height = th;
        const tctx = tmp.getContext("2d");
        if (tctx) {
          tctx.drawImage(base, x, y, rw, rh, 0, 0, tw, th);
          ctx.imageSmoothingEnabled = false;
          ctx.drawImage(tmp, 0, 0, tw, th, x, y, rw, rh);
          ctx.imageSmoothingEnabled = true;
        }
      }
      ctx.restore();
      continue;
    }

    if (s.tool === "arrow" || s.tool === "rect") {
      const a = pts[0];
      const b = pts[pts.length - 1];
      if (s.tool === "rect") {
        ctx.strokeRect(Math.min(a.x, b.x), Math.min(a.y, b.y), Math.abs(b.x - a.x), Math.abs(b.y - a.y));
      } else {
        ctx.beginPath();
        ctx.moveTo(a.x, a.y);
        ctx.lineTo(b.x, b.y);
        ctx.stroke();
        // Arrowhead.
        const ang = Math.atan2(b.y - a.y, b.x - a.x);
        const len = 16 * scale;
        ctx.beginPath();
        ctx.moveTo(b.x, b.y);
        ctx.lineTo(b.x - len * Math.cos(ang - 0.4), b.y - len * Math.sin(ang - 0.4));
        ctx.moveTo(b.x, b.y);
        ctx.lineTo(b.x - len * Math.cos(ang + 0.4), b.y - len * Math.sin(ang + 0.4));
        ctx.stroke();
      }
    } else {
      ctx.beginPath();
      ctx.moveTo(pts[0].x, pts[0].y);
      for (let i = 1; i < pts.length; i++) ctx.lineTo(pts[i].x, pts[i].y);
      ctx.stroke();
    }
    ctx.restore();
  }
}

createRoot(document.getElementById("root")!).render(<EditorApp />);
