import { useCallback, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { emit, listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

type Tool = "pen" | "highlighter" | "arrow" | "rect";

interface Stroke {
  tool: Tool;
  color: string;
  points: { x: number; y: number }[];
}

const PALETTE = ["#ff4d4f", "#ffd43b", "#4dabf7", "#69db7c", "#ffffff", "#000000"];
const TOOL_WIDTH: Record<Tool, number> = {
  pen: 3,
  highlighter: 22,
  arrow: 3,
  rect: 3,
};

const TOOLS: { id: Tool; label: string; glyph: string }[] = [
  { id: "pen", label: "Pen", glyph: "✏️" },
  { id: "highlighter", label: "Highlighter", glyph: "🖍️" },
  { id: "arrow", label: "Arrow", glyph: "➜" },
  { id: "rect", label: "Rectangle", glyph: "▭" },
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
    drawStrokes(ctx, strokesRef.current, canvas.width, canvas.height);
  }, []);

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

  const toImgPoint = (e: React.PointerEvent): { x: number; y: number } => {
    const canvas = canvasRef.current!;
    const rect = canvas.getBoundingClientRect();
    const sx = canvas.width / rect.width;
    const sy = canvas.height / rect.height;
    return { x: (e.clientX - rect.left) * sx, y: (e.clientY - rect.top) * sy };
  };

  const onPointerDown = (e: React.PointerEvent) => {
    if (!ready) return;
    e.preventDefault();
    canvasRef.current?.setPointerCapture(e.pointerId);
    const stroke: Stroke = { tool, color, points: [toImgPoint(e)] };
    activeRef.current.set(e.pointerId, stroke);
  };

  const onPointerMove = (e: React.PointerEvent) => {
    if (!activeRef.current.has(e.pointerId)) return;
    const stroke = activeRef.current.get(e.pointerId)!;
    stroke.points.push(toImgPoint(e));
    commitStroke(stroke);
  };

  const onPointerUp = (e: React.PointerEvent) => {
    const stroke = activeRef.current.get(e.pointerId);
    if (!stroke) return;
    activeRef.current.delete(e.pointerId);
    if (stroke.points.length > 1) {
      setStrokes((s) => [...s, stroke]);
    } else {
      commitStroke(stroke);
    }
  };

  const commitStroke = (stroke: Stroke) => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const base = baseImageRef.current;
    if (base) ctx.drawImage(base, 0, 0, canvas.width, canvas.height);
    drawStrokes(ctx, [...strokesRef.current, stroke], canvas.width, canvas.height);
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
    const blob = await new Promise<Blob | null>((res) => canvas.toBlob(res, "image/png"));
    confirmingRef.current = false;
    if (!blob) return;
    const reader = new FileReader();
    reader.onload = () => {
      void emit("editor-confirmed", { annotated: reader.result as string });
    };
    reader.readAsDataURL(blob);
  }, []);

  return (
    <div className="editor-root" ref={wrapRef}>
      <div className="editor-toolbar" role="toolbar" aria-label="Annotation tools">
        {TOOLS.map((t) => (
          <button
            key={t.id}
            type="button"
            className={`tool-btn${tool === t.id ? " active" : ""}`}
            title={t.label}
            onClick={() => setTool(t.id)}
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
        <canvas
          ref={canvasRef}
          className="editor-canvas"
          style={{
            width: fit.w * fit.scale,
            height: fit.h * fit.scale,
            touchAction: "none",
          }}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerUp}
        />
      </div>

      <div className="editor-hint">
        <span>Enter <b>✓</b> confirm</span>
        <span>Esc <b>✕</b> skip annotations</span>
        <span>Ctrl+Z undo</span>
      </div>
    </div>
  );
}

function drawStrokes(ctx: CanvasRenderingContext2D, strokes: Stroke[], w: number, h: number) {
  const sx = w / (ctx.canvas.width || w);
  const sy = h / (ctx.canvas.height || h);
  for (const s of strokes) {
    ctx.save();
    ctx.lineJoin = "round";
    ctx.lineCap = "round";
    ctx.strokeStyle = s.color;
    ctx.fillStyle = s.color;
    ctx.globalAlpha = s.tool === "highlighter" ? 0.45 : 1;
    ctx.lineWidth = (TOOL_WIDTH[s.tool] || 3) * Math.min(sx, sy);
    const pts = s.points;
    if (pts.length < 2) continue;
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
        const len = 16 * Math.min(sx, sy);
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
