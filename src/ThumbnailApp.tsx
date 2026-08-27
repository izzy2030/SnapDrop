import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, CapturedPayload, ToastPayload } from "./api";

interface StackItem {
  key: number;
  captureId: number;
  path: string | null;
  preview: string;
  width: number;
  height: number;
  unsaved: boolean;
}

let nextKey = 1;

function toStackItem(p: CapturedPayload): StackItem {
  return {
    key: nextKey++,
    captureId: p.capture_id,
    path: p.path,
    preview: p.preview,
    width: p.width,
    height: p.height,
    unsaved: p.unsaved,
  };
}

function fileName(p: string) {
  return p.split(/[\\/]/).pop() ?? p;
}

export default function ThumbnailApp() {
  const [stack, setStack] = useState<StackItem[]>([]);
  const [expanded, setExpanded] = useState(false);
  const [toast, setToast] = useState<ToastPayload | null>(null);
  const toastTimer = useRef<number | null>(null);
  const lastClickRef = useRef(0);
  const seenCaptureIdsRef = useRef(new Set<number>());
  const dismissedCaptureIdsRef = useRef(new Set<number>());
  const reconcileInFlightRef = useRef(false);

  const removeFromStack = useCallback((path: string) => {
    setStack((s) => {
      const removed = s.filter((item) => item.path === path);
      for (const item of removed) {
        dismissedCaptureIdsRef.current.add(item.captureId);
      }
      return s.filter((item) => item.path !== path);
    });
  }, []);

  const addCapture = useCallback((p: CapturedPayload) => {
    const id = p.capture_id;
    api.debugLog(`addCapture id=${id} preview=${p.preview ? p.preview.length : 0} path=${p.path}`).catch(() => {});
    if (!p.preview || !Number.isSafeInteger(id) || id <= 0) {
      api.debugLog(`addCapture REJECT id=${id} (invalid payload)`).catch(() => {});
      return;
    }
    if (dismissedCaptureIdsRef.current.has(id)) {
      api.debugLog(`addCapture SKIP id=${id} (dismissed)`).catch(() => {});
      return;
    }
    if (seenCaptureIdsRef.current.has(id)) {
      api.debugLog(`addCapture SKIP id=${id} (already seen)`).catch(() => {});
      return;
    }
    seenCaptureIdsRef.current.add(id);
    setStack((s) => [toStackItem(p), ...s].slice(0, 10));
    setExpanded(false);
    api.debugLog(`addCapture ACCEPT id=${id} -> new stack top`).catch(() => {});
  }, []);

  useEffect(() => {
    // Reconcile from the backend as well as listening for events. A renderer
    // can miss a one-shot event while WebView2 is resuming after display sleep.
    const reconcileLatest = () => {
      const startedAt = performance.now();
      api.debugLog(`reconcile begin pending=${reconcileInFlightRef.current}`).catch(() => {});
      // IPC can remain pending while WebView2 is recovering. Do not build an
      // unbounded queue of calls every 1.5s; one probe at a time also keeps the
      // renderer responsive after a delayed IPC reply.
      if (reconcileInFlightRef.current) return;
      reconcileInFlightRef.current = true;
      void api
        .getLatestCapture()
        .then((p) => {
          api.debugLog(`reconcile success elapsed_ms=${Math.round(performance.now() - startedAt)} id=${p ? p.capture_id : "null"} preview_len=${p?.preview?.length ?? 0}`).catch(() => {});
          if (p) addCapture(p);
        })
        .catch((e) => {
          api.debugLog(`reconcile ERROR elapsed_ms=${Math.round(performance.now() - startedAt)} ${String(e)}`).catch(() => {});
        })
        .finally(() => {
          reconcileInFlightRef.current = false;
          api.debugLog(`reconcile end elapsed_ms=${Math.round(performance.now() - startedAt)}`).catch(() => {});
        });
    };
    api.debugLog(`renderer mounted href=${window.location.href} visibility=${document.visibilityState} dpr=${window.devicePixelRatio}`).catch(() => {});
    reconcileLatest();
    const reconcileTimer = window.setInterval(reconcileLatest, 1500);

    const unCaptured = listen<CapturedPayload>("thumbnail-captured", (e) => {
      api.debugLog(`event thumbnail-captured id=${e.payload.capture_id}`).catch(() => {});
      addCapture(e.payload);
    });
    const unToast = listen<ToastPayload>("toast", (e) => {
      setToast(e.payload);
      if (toastTimer.current) window.clearTimeout(toastTimer.current);
      toastTimer.current = window.setTimeout(() => setToast(null), 4000);
    });
    return () => {
      window.clearInterval(reconcileTimer);
      unCaptured.then((f) => f());
      unToast.then((f) => f());
    };
  }, [addCapture]);

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
    api.debugLog(`drag start path=${path}`).catch(() => {});
    api
      .startDrag(path)
      .then((outcome) => {
        api.debugLog(`drag done path=${path} dropped=${outcome.dropped} moved=${outcome.moved}`).catch(() => {});
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
        api.debugLog(`drag ERROR ${String(e)}`).catch(() => {});
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
    if (e.button !== 0 || !path) {
      api.debugLog(`pointerdown ignored (button=${e.button} path=${path})`).catch(() => {});
      return;
    }
    // Modifier-clicks never start a drag.
    if (e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) {
      api.debugLog(`pointerdown ignored (modifier ctrl=${e.ctrlKey} meta=${e.metaKey} shift=${e.shiftKey} alt=${e.altKey})`).catch(() => {});
      return;
    }
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

  useEffect(() => {
    const reportVisibility = () => {
      api.debugLog(`renderer visibility=${document.visibilityState} hidden=${document.hidden} stack=${stack.length} current_id=${stack[0]?.captureId ?? 0}`).catch(() => {});
    };
    document.addEventListener("visibilitychange", reportVisibility);
    window.addEventListener("pageshow", reportVisibility);
    window.addEventListener("pagehide", reportVisibility);
    window.addEventListener("error", (e) => api.debugLog(`renderer window.error message=${e.message} source=${e.filename}:${e.lineno}:${e.colno}`).catch(() => {}));
    window.addEventListener("unhandledrejection", (e) => api.debugLog(`renderer unhandledrejection reason=${String(e.reason)}`).catch(() => {}));
    return () => {
      document.removeEventListener("visibilitychange", reportVisibility);
      window.removeEventListener("pageshow", reportVisibility);
      window.removeEventListener("pagehide", reportVisibility);
    };
  }, [stack]);

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
