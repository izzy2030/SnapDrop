import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, CapturedPayload, ToastPayload } from "./api";

interface StackItem {
  key: number;
  path: string | null;
  preview: string;
  width: number;
  height: number;
  unsaved: boolean;
}

let nextKey = 1;

function fileName(p: string) {
  return p.split(/[\\/]/).pop() ?? p;
}

export default function ThumbnailApp() {
  const [stack, setStack] = useState<StackItem[]>([]);
  const [expanded, setExpanded] = useState(false);
  const [toast, setToast] = useState<ToastPayload | null>(null);
  const toastTimer = useRef<number | null>(null);
  const lastClickRef = useRef(0);

  const removeFromStack = useCallback((path: string) => {
    setStack((s) => s.filter((i) => i.path !== path));
  }, []);

  useEffect(() => {
    // Check for pending/latest capture on mount
    api.getLatestCapture()
      .then((p) => {
        if (p && p.preview) {
          setStack((s) => {
            if (s.length > 0) return s;
            return [{ key: nextKey++, path: p.path, preview: p.preview, width: p.width, height: p.height, unsaved: p.unsaved }];
          });
        }
      })
      .catch(() => {});

    const unCaptured = listen<CapturedPayload>("captured", (e) => {
      const p = e.payload;
      setStack((s) =>
        [{ key: nextKey++, path: p.path, preview: p.preview, width: p.width, height: p.height, unsaved: p.unsaved }, ...s].slice(0, 10),
      );
      setExpanded(false);
    });
    const unToast = listen<ToastPayload>("toast", (e) => {
      setToast(e.payload);
      if (toastTimer.current) window.clearTimeout(toastTimer.current);
      toastTimer.current = window.setTimeout(() => setToast(null), 4000);
    });
    return () => {
      unCaptured.then((f) => f());
      unToast.then((f) => f());
    };
  }, []);

  // Auto-dismiss per settings.
  useEffect(() => {
    const current = stack[0];
    if (!current) return;
    let timer: number | undefined;
    api
      .getSettings()
      .then((s) => {
        if (s.thumbnail_duration_secs > 0 && !expanded) {
          timer = window.setTimeout(() => api.hideThumbnail(), s.thumbnail_duration_secs * 1000);
        }
      })
      .catch(() => {});
    return () => {
      if (timer) window.clearTimeout(timer);
    };
  }, [stack, expanded]);

  const current = stack[0];

  // The native drag is initiated immediately on grab; the Rust drag thread
  // decides whether the gesture becomes a real drag (cursor movement) or a
  // plain click (release without moving). Ctrl+click is reserved for "show in
  // folder" and never starts a drag.
  const startNativeDrag = (path: string | null) => {
    if (!path) return;
    api
      .startDrag(path)
      .then((outcome) => {
        if (outcome.moved) {
          removeFromStack(path);
        }
        // A successful drop into any app delivers the file — dismiss the
        // thumbnail unless the user wants to keep it around (settings).
        if (outcome.dropped) {
          api.getSettings().then((s) => {
            if (s.hide_after_drop) api.hideThumbnail();
          });
        }
      })
      .catch((e) => {
        setToast({ kind: "error", message: `Couldn't start drag: ${e}` });
        if (toastTimer.current) window.clearTimeout(toastTimer.current);
        toastTimer.current = window.setTimeout(() => setToast(null), 4000);
      });
  };

  // Left-button gestures on the thumbnail surface:
  //   plain press+move        → native file drag
  //   Ctrl+click              → reveal in Explorer (handled on click, after release)
  //   double-click            → open with the default image app
  const onPointerDown = (e: React.PointerEvent, path: string | null) => {
    if (e.button !== 0 || !path) return;
    // Modifier-clicks never start a drag.
    if (e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    const now = Date.now();
    if (now - lastClickRef.current < 350) {
      lastClickRef.current = 0; // consume the double-click
      api.openCapture(path);
      return;
    }
    lastClickRef.current = now;
    startNativeDrag(path);
  };

  const onCardClick = (e: React.MouseEvent, path: string | null) => {
    if (!path) return;
    // Ctrl+click (or Ctrl+double-click) → reveal in Explorer.
    if (e.ctrlKey || e.metaKey) {
      e.preventDefault();
      e.stopPropagation();
      api.revealCapture(path);
      return;
    }
    // A double-click is "open", not "expand the stack".
    const now = Date.now();
    if (now - lastClickRef.current > 500) {
      if (stack.length > 1 && !expanded) setExpanded(true);
    }
  };

  return (
    <div className="thumbnail-root">
      {toast && <div className={`toast ${toast.kind}`}>{toast.message}</div>}

      {current && (
        <div
          className={`thumb-stack${expanded ? " expanded" : ""}`}
          title={stack.length > 1 ? "Click to expand recent captures" : undefined}
        >
          {/* Older captures peek out behind the primary thumbnail */}
          {stack.slice(1, 3).map((item, i) => (
            <div
              key={item.key}
              className="thumb-peek"
              style={{ transform: `scale(${1 - (i + 1) * 0.05}) translateY(${(i + 1) * 6}px)` }}
            >
              <img src={item.preview} alt="" draggable={false} />
            </div>
          ))}

          <div
            className={`thumb-card${current.unsaved ? " unsaved" : ""}`}
            onPointerDown={(e) => onPointerDown(e, current.path)}
            onClick={(e) => onCardClick(e, current.path)}
          >
            {current.path ? (
              <img className="thumb-img" src={current.preview} alt="capture" draggable={false} />
            ) : (
              <div className="thumb-img thumb-placeholder" />
            )}
            {current.unsaved && <div className="unsaved-badge">Not saved</div>}
            <div className="thumb-topbar">
              <span className="thumb-caption">{fileName(current.path ?? "unsaved")}</span>
            </div>
          </div>
        </div>
      )}

      {expanded && stack.length > 1 && (
        <div className="expanded-list">
          {stack.slice(1).map((item) => (
            <div
              key={item.key}
              className="expanded-item"
              onPointerDown={(e) => onPointerDown(e, item.path)}
            >
              <img src={item.preview} alt="" draggable={false} />
              <div className="expanded-info">
                <span className="expanded-name">{fileName(item.path ?? "unsaved")}</span>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
