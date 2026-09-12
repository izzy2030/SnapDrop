import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, CapturedPayload, ToastPayload } from "./api";
import { installWindowDiagnostics } from "./diag";

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
  const lastInputAtRef = useRef(0);
  const lastInputReportAtRef = useRef(0);
  // Frame-staleness probe (re-arming rAF loop below): performance.now() of the
  // last composed frame, 0 until the first. A one-shot probe would measure
  // page age, not staleness — this must re-arm every frame.
  const lastFrameAtRef = useRef(0);
  const frameStalledRef = useRef(false);
  // Hidden-interval clock: both wall and monotonic stamps taken on hide; on
  // show the hidden duration is max(wall Δ, perf Δ) so an OS time correction
  // across sleep cannot corrupt the ordering.
  const hiddenAtRef = useRef<{ wall: number; perf: number } | null>(null);
  const lastReconcileIdRef = useRef(0);

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
          // Transition-only: the 1.5s poll succeeding with the same capture
          // is not information. Log only when the id changes (or is null).
          const id = p ? p.capture_id : 0;
          if (id !== lastReconcileIdRef.current) {
            lastReconcileIdRef.current = id;
            api.debugLog(`reconcile success elapsed_ms=${Math.round(performance.now() - startedAt)} id=${p ? p.capture_id : "null"} preview_len=${p?.preview?.length ?? 0}`).catch(() => {});
          }
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
    // WebView2 can drop the very first IPC call from a freshly created page
    // (observed: the first debug_log invoke never reaches the backend, in every
    // session). Send a throwaway probe first so the real diagnostics below are
    // guaranteed to land in the log.
    api.debugLog("ipc warmup").catch(() => {});
    // Mount is logged by installWindowDiagnostics below (tag "thumb").
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
      .startDrag([path])
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

  // Input-liveness probe: report (throttled) that the page is receiving user
  // input. The Rust watchdog treats a fresh heartbeat with no input for a long
  // time while the window is on screen as a "ghost" (stale frame, dead drags)
  // and reloads the webview. Also logs the input age so a stuck page shows up
  // in the diagnostics.
  useEffect(() => {
    const onInput = (e: Event) => {
      lastInputAtRef.current = Date.now();
      // A pointerdown is the exact "user tried to grab it" signal: report it
      // immediately (unthrottled) so the Rust watchdog can detect a click
      // that never turns into a drag (the ghost). Only left-button presses
      // without modifiers are potential drag attempts — the Rust watchdog
      // treats a reported pointerdown as "a drag should follow", so right-
      // clicks / modifier-clicks (which never drag) must not count.
      if (e.type === "pointerdown") {
        const pe = e as PointerEvent;
        const dragAttempt =
          pe.button === 0 && !pe.ctrlKey && !pe.metaKey && !pe.shiftKey && !pe.altKey;
        if (dragAttempt) {
          api.reportRendererPointerDown().catch(() => {});
        }
        api
          .debugLog(
            `renderer pointerdown button=${pe.button} ctrl=${pe.ctrlKey} meta=${pe.metaKey} shift=${pe.shiftKey} alt=${pe.altKey} drag_attempt=${dragAttempt}`
          )
          .catch(() => {});
      }
      if (Date.now() - lastInputReportAtRef.current >= 2000) {
        lastInputReportAtRef.current = Date.now();
        api.reportRendererInput().catch(() => {});
      }
    };
    window.addEventListener("pointerdown", onInput, true);
    window.addEventListener("pointermove", onInput, true);
    window.addEventListener("keydown", onInput, true);
    window.addEventListener("wheel", onInput, true);
    window.addEventListener("touchstart", onInput, true);
    return () => {
      window.removeEventListener("pointerdown", onInput, true);
      window.removeEventListener("pointermove", onInput, true);
      window.removeEventListener("keydown", onInput, true);
      window.removeEventListener("wheel", onInput, true);
      window.removeEventListener("touchstart", onInput, true);
    };
  }, []);

  // Frame-staleness loop: re-arms every frame. If frames stop (compositor
  // frozen while JS timers/IPC keep running — the ghost state), the watchdog
  // cannot tell from the heartbeat alone; frame_age_s in the lines below can.
  useEffect(() => {
    let alive = true;
    let raf = 0;
    const tick = () => {
      if (!alive) return;
      const now = performance.now();
      if (lastFrameAtRef.current !== 0 && now - lastFrameAtRef.current > 5000 && !frameStalledRef.current) {
        frameStalledRef.current = true;
        api.debugLog(`renderer frames STALLED gap_s=${((now - lastFrameAtRef.current) / 1000).toFixed(0)}`).catch(() => {});
      } else if (frameStalledRef.current && now - lastFrameAtRef.current <= 5000) {
        frameStalledRef.current = false;
        api.debugLog("renderer frames resumed").catch(() => {});
      }
      lastFrameAtRef.current = now;
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => {
      alive = false;
      cancelAnimationFrame(raf);
    };
  }, []);

  useEffect(() => {
    // Snapshot of exactly the flags that gate this feature, attached to
    // lifecycle lines and captured errors so either is interpretable alone.
    const snap = () => {
      const frameAge =
        lastFrameAtRef.current === 0
          ? "n/a"
          : `${Math.round((performance.now() - lastFrameAtRef.current) / 1000)}`;
      return `stack=${stack.length} current_id=${stack[0]?.captureId ?? 0} expanded=${expanded} input_age_s=${Math.round((Date.now() - lastInputAtRef.current) / 1000)} frame_age_s=${frameAge}`;
    };
    const onVisibility = () => {
      if (document.hidden) {
        hiddenAtRef.current = { wall: Date.now(), perf: performance.now() };
        api.debugLog(`renderer lifecycle event=hidden ${snap()}`).catch(() => {});
      } else {
        const h = hiddenAtRef.current;
        hiddenAtRef.current = null;
        let dur = "unknown";
        if (h) {
          const wall = (Date.now() - h.wall) / 1000;
          const perf = (performance.now() - h.perf) / 1000;
          dur = `${Math.max(wall, perf).toFixed(1)}s (wall=${wall.toFixed(1)} perf=${perf.toFixed(1)})`;
        }
        api.debugLog(`renderer lifecycle event=visible hidden_for=${dur} ${snap()}`).catch(() => {});
      }
    };
    const onShow = () => api.debugLog(`renderer lifecycle event=pageshow ${snap()}`).catch(() => {});
    const onHide = () => api.debugLog(`renderer lifecycle event=pagehide ${snap()}`).catch(() => {});
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("pageshow", onShow);
    window.addEventListener("pagehide", onHide);
    const uninstallErrors = installWindowDiagnostics("thumb", snap);
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("pageshow", onShow);
      window.removeEventListener("pagehide", onHide);
      uninstallErrors();
    };
  }, [stack, expanded]);

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
