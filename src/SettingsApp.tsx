import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, HistoryEntry, Settings } from "./api";

type NavTab = "gallery" | "settings" | "about";
type CanvasFilter = "all" | "today";
type ViewMode = "list" | "grid";
type Status = { kind: "info" | "error"; text: string } | null;

function keyName(code: string): string | null {
  if (code.startsWith("Digit")) return code.slice(5);
  if (code.startsWith("Key")) return code.slice(3).toLowerCase();
  if (/^F\d{1,2}$/.test(code)) return code;
  switch (code) {
    case "Space":
      return "Space";
    case "Enter":
      return "Enter";
    case "Tab":
      return "Tab";
    case "Escape":
      return "Esc";
    case "Backspace":
      return "Backspace";
    case "Delete":
      return "Delete";
    case "Home":
      return "Home";
    case "End":
      return "End";
    case "PageUp":
      return "PageUp";
    case "PageDown":
      return "PageDown";
    case "Insert":
      return "Insert";
    case "ArrowUp":
      return "Up";
    case "ArrowDown":
      return "Down";
    case "ArrowLeft":
      return "Left";
    case "ArrowRight":
      return "Right";
    default:
      return null;
  }
}

function HotkeyField({
  value,
  onChange,
}: {
  value: string;
  onChange: (v: string) => void;
}) {
  const [recording, setRecording] = useState(false);

  return (
    <button
      type="button"
      className={`hotkey-field${recording ? " recording" : ""}`}
      onClick={() => setRecording(true)}
      onKeyDown={(e) => {
        if (!recording) return;
        e.preventDefault();
        e.stopPropagation();
        const mods: string[] = [];
        if (e.ctrlKey) mods.push("Ctrl");
        if (e.shiftKey) mods.push("Shift");
        if (e.altKey) mods.push("Alt");
        if (e.metaKey) mods.push("Win");
        const key = keyName(e.code);
        if (key) {
          const combo = [...mods, key].join("+");
          if (combo !== "+") {
            onChange(combo);
          }
          setRecording(false);
        }
      }}
      onBlur={() => setRecording(false)}
      title="Click, then press the new shortcut"
    >
      {recording ? "Press keys…" : value || "Set shortcut"}
    </button>
  );
}

function fileName(p: string) {
  return p.split(/[\\/]/).pop() ?? p;
}

function formatBytes(bytes?: number): string {
  if (!bytes || bytes === 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatDate(iso: string): string {
  if (!iso) return "";
  try {
    const d = new Date(iso);
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  } catch {
    return iso.slice(11, 19);
  }
}

function formatDuration(secs?: number): string {
  const s = Math.max(0, Math.floor(secs ?? 0));
  const pad = (n: number) => String(n).padStart(2, "0");
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  return h > 0 ? `${pad(h)}:${pad(m)}:${pad(ss)}` : `${pad(m)}:${pad(ss)}`;
}

function isVideoEntry(item: HistoryEntry): boolean {
  if (item.kind === "video") return true;
  return /\.(mp4|mkv|webm|avi|mov)$/i.test(item.path);
}

function PlayIcon() {
  return (
    <svg width="22" height="22" viewBox="0 0 24 24" fill="currentColor" aria-hidden>
      <path d="M8 5v14l11-7z" />
    </svg>
  );
}

/**
 * Thumbnail for a history entry. Videos get a real frame (when the preview is
 * available) with a play badge + duration overlay on top; without a preview
 * they fall back to the dark placeholder card. Images render as-is.
 */
function PreviewThumb({
  item,
  preview,
  className,
}: {
  item: HistoryEntry;
  preview?: string;
  className?: string;
}) {
  if (isVideoEntry(item)) {
    return (
      <div className={`video-thumb ${className ?? ""}`}>
        {preview ? (
          <img src={preview} alt={fileName(item.path)} className={`${className ?? ""} video-frame`} />
        ) : null}
        <span className="video-play-badge">
          <PlayIcon />
        </span>
      </div>
    );
  }
  if (preview) {
    return <img src={preview} alt={fileName(item.path)} className={className} />;
  }
  return <span className="screenshot-thumb-placeholder">PNG</span>;
}

function isToday(iso: string): boolean {
  if (!iso) return false;
  try {
    const d = new Date(iso);
    const today = new Date();
    return (
      d.getDate() === today.getDate() &&
      d.getMonth() === today.getMonth() &&
      d.getFullYear() === today.getFullYear()
    );
  } catch {
    return false;
  }
}

export default function SettingsApp() {
  const [navTab, setNavTab] = useState<NavTab>("gallery");
  const [canvasFilter, setCanvasFilter] = useState<CanvasFilter>("all");
  const [viewMode, setViewMode] = useState<ViewMode>("list");
  const [searchQuery, setSearchQuery] = useState("");

  const [settings, setSettings] = useState<Settings | null>(null);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [previews, setPreviews] = useState<Record<string, string>>({});
  const [refreshing, setRefreshing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<Status>(null);
  const [version, setVersion] = useState<string>("");
  const [showMoreMenu, setShowMoreMenu] = useState(false);
  const [debugLog, setDebugLog] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const moreMenuRef = useRef<HTMLDivElement>(null);
  // Capture files are immutable — a path's preview only needs to be fetched
  // once per session. Re-fetching all of them on every refresh/focus made the
  // main window stall at grab time (the "focus" event fires exactly when the
  // user presses the title bar to drag it).
  const fetchedPreviews = useRef<Set<string>>(new Set());
  const selectionAnchor = useRef<number | null>(null);

  const clearSelection = useCallback(() => {
    setSelected(new Set());
    selectionAnchor.current = null;
  }, []);

  useEffect(() => {
    const handleOutsideClick = (e: MouseEvent) => {
      if (moreMenuRef.current && !moreMenuRef.current.contains(e.target as Node)) {
        setShowMoreMenu(false);
      }
    };
    if (showMoreMenu) {
      document.addEventListener("mousedown", handleOutsideClick);
    }
    return () => {
      document.removeEventListener("mousedown", handleOutsideClick);
    };
  }, [showMoreMenu]);

  // Esc is a GLOBAL hotkey in Rust (dismisses overlay/thumbnail/editor) that
  // consumes the key — the webview never sees the keydown itself. Rust
  // re-emits it as "esc-pressed"; the keydown listener below is only a
  // fallback for running the UI in a plain browser.
  useEffect(() => {
    const onEsc = () => {
      setShowMoreMenu(false);
      clearSelection();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onEsc();
    };
    document.addEventListener("keydown", onKey);
    const unlisten = listen("esc-pressed", onEsc);
    return () => {
      document.removeEventListener("keydown", onKey);
      unlisten.then((f) => f());
    };
  }, [clearSelection]);

  const refreshHistory = useCallback(() => {
    setRefreshing(true);
    api
      .getHistory()
      .then((h) => {
        setHistory(h);
        // Drop selections for entries that no longer exist (e.g. files
        // deleted outside the app).
        setSelected((prev) => {
          if (prev.size === 0) return prev;
          const live = new Set(h.map((e) => e.path));
          const next = new Set([...prev].filter((p) => live.has(p)));
          return next.size === prev.size ? prev : next;
        });
        // Drop previews for entries that no longer exist (e.g. files deleted
        // outside the app), and fetch previews for visible entries.
        setPreviews((prev) => {
          const live = new Set(h.map((e) => e.path));
          const next: Record<string, string> = {};
          for (const [k, v] of Object.entries(prev)) {
            if (live.has(k)) next[k] = v;
          }
          // Keep the fetched-set in sync so deleted paths could be re-fetched
          // if they reappear (unlikely) and new paths get fetched below.
          for (const k of Array.from(fetchedPreviews.current)) {
            if (!live.has(k)) fetchedPreviews.current.delete(k);
          }
          return next;
        });
        h.forEach((entry) => {
          if (fetchedPreviews.current.has(entry.path)) return;
          fetchedPreviews.current.add(entry.path);
          api
            .getCapturePreview(entry.path)
            .then((preview) => {
              // An empty result means the preview couldn't be built (e.g. a
              // video's shell thumbnail raced file finalization). Treat it like
              // an error so this path is retried on the next refresh instead of
              // being marked fetched against a blank frame forever.
              if (!preview) {
                fetchedPreviews.current.delete(entry.path);
                return;
              }
              setPreviews((prev) => {
                if (prev[entry.path] === preview) return prev;
                return { ...prev, [entry.path]: preview };
              });
            })
            .catch(() => {
              // Allow a retry on the next refresh if the read failed.
              fetchedPreviews.current.delete(entry.path);
            });
        });
      })
      .catch((e) => console.error("History fetch error:", e))
      .finally(() => setRefreshing(false));
  }, []);

  useEffect(() => {
    api.getSettings().then(setSettings).catch((e) => setStatus({ kind: "error", text: String(e) }));
    refreshHistory();
    api.getAppVersion().then(setVersion).catch(() => {});

    const unCaptured = listen("thumbnail-captured", () => refreshHistory());
    const unHistory = listen("history-updated", () => refreshHistory());
    // Notifications from Rust (capture results, OCR results, errors). The
    // floating thumbnail used to be the only listener; show them here too so
    // they are visible whenever the main window is open.
    const unToast = listen<{ kind: "info" | "error" | "success"; message: string }>("toast", (e) => {
      setStatus({ kind: e.payload.kind === "error" ? "error" : "info", text: e.payload.message });
      window.setTimeout(() => setStatus(null), 5000);
    });
    const onFocus = () => refreshHistory();
    window.addEventListener("focus", onFocus);

    return () => {
      unCaptured.then((f) => f());
      unHistory.then((f) => f());
      unToast.then((f) => f());
      window.removeEventListener("focus", onFocus);
    };
  }, [refreshHistory]);

  const set = (patch: Partial<Settings>) => {
    setSettings((s) => (s ? { ...s, ...patch } : s));
  };

  const save = async () => {
    if (!settings) return;
    setSaving(true);
    setStatus(null);
    try {
      await api.updateSettings(settings);
      setStatus({ kind: "info", text: "Settings saved successfully." });
      setTimeout(() => setStatus(null), 3000);
    } catch (e) {
      setStatus({ kind: "error", text: String(e) });
    } finally {
      setSaving(false);
    }
  };


  const filteredHistory = useMemo(() => {
    return history.filter((item) => {
      const name = fileName(item.path).toLowerCase();
      const matchesSearch = searchQuery ? name.includes(searchQuery.toLowerCase()) : true;
      const matchesFilter = canvasFilter === "today" ? isToday(item.captured_at) : true;
      return matchesSearch && matchesFilter;
    });
  }, [history, searchQuery, canvasFilter]);

  const groupedHistory = useMemo(() => {
    const today: HistoryEntry[] = [];
    const earlier: HistoryEntry[] = [];
    filteredHistory.forEach((item) => {
      if (isToday(item.captured_at)) {
        today.push(item);
      } else {
        earlier.push(item);
      }
    });
    return { today, earlier };
  }, [filteredHistory]);

  const pathIndex = useMemo(() => {
    const m = new Map<string, number>();
    filteredHistory.forEach((h, i) => m.set(h.path, i));
    return m;
  }, [filteredHistory]);

  const toggleSelect = (e: React.MouseEvent, item: HistoryEntry, idx: number) => {
    e.preventDefault();
    e.stopPropagation();
    if (e.shiftKey && selectionAnchor.current !== null) {
      const lo = Math.min(selectionAnchor.current, idx);
      const hi = Math.max(selectionAnchor.current, idx);
      setSelected((prev) => {
        const next = new Set(prev);
        for (let i = lo; i <= hi; i++) {
          const p = filteredHistory[i]?.path;
          if (p) next.add(p);
        }
        return next;
      });
    } else {
      setSelected((prev) => {
        const next = new Set(prev);
        if (next.has(item.path)) next.delete(item.path);
        else next.add(item.path);
        return next;
      });
      selectionAnchor.current = idx;
    }
  };

  const handleRowMouseDown = (e: React.MouseEvent, item: HistoryEntry) => {
    if (e.button !== 0) return;
    // Ctrl/Shift+press selects instead of dragging.
    if (e.ctrlKey || e.shiftKey) {
      const idx = pathIndex.get(item.path) ?? -1;
      toggleSelect(e, item, idx);
      return;
    }
    // Pressing a row that is part of a multi-selection drags the whole set;
    // any other row drags just that file (and clears the selection).
    const multi = selected.size > 1 && selected.has(item.path);
    const dragging = multi ? Array.from(selected) : [item.path];
    if (!multi) clearSelection();
    api
      .startDrag(dragging)
      .then((outcome) => {
        if (outcome.moved) {
          clearSelection();
          refreshHistory();
        }
      })
      .catch(console.error);
  };

  if (!settings) {
    return (
      <div className="empty-state" style={{ height: "100vh" }}>
        <p>Loading SnapDrop…</p>
      </div>
    );
  }

  return (
    <div className="app-container">
      {/* Left Navigation Sidebar */}
      <aside className="app-sidebar">
        {/* Brand Header */}
        <div className="sidebar-header">
          <img src="/assets/logo-full.png" alt="SnapDrop" className="sidebar-brand-logo" />
        </div>

        {/* Hero Capture Button */}
        <div className="sidebar-action-wrap">
          <button
            type="button"
            className="btn-capture-hero"
            onClick={() => api.captureNow()}
            title="Capture screen region"
          >
            <div className="hero-left">
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round">
                <path d="M23 19a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4l2-3h6l2 3h4a2 2 0 0 1 2 2z"/>
                <circle cx="12" cy="13" r="4"/>
              </svg>
              <span>Screenshot</span>
            </div>
            <span className="hero-shortcut">{settings.hotkey || "Ctrl+Alt+S"}</span>
          </button>
        </div>

        {/* Sidebar Nav Items */}
        <nav className="sidebar-nav">
          <button
            type="button"
            className={`nav-item ${navTab === "gallery" ? "active" : ""}`}
            onClick={() => setNavTab("gallery")}
          >
            <div className="nav-item-left">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <rect x="3" y="3" width="18" height="18" rx="2" ry="2"/>
                <circle cx="8.5" cy="8.5" r="1.5"/>
                <polyline points="21 15 16 10 5 21"/>
              </svg>
              <span>History</span>
            </div>
            <span className="nav-badge">{history.length}</span>
          </button>

          <button
            type="button"
            className={`nav-item ${navTab === "settings" ? "active" : ""}`}
            onClick={() => setNavTab("settings")}
          >
            <div className="nav-item-left">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <circle cx="12" cy="12" r="3"/>
                <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"/>
              </svg>
              <span>Settings</span>
            </div>
          </button>

          <button
            type="button"
            className={`nav-item ${navTab === "about" ? "active" : ""}`}
            onClick={() => setNavTab("about")}
          >
            <div className="nav-item-left">
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <circle cx="12" cy="12" r="10"/>
                <line x1="12" y1="16" x2="12" y2="12"/>
                <line x1="12" y1="8" x2="12.01" y2="8"/>
              </svg>
              <span>About & Help</span>
            </div>
          </button>
        </nav>

        {/* Sidebar Footer */}
        <div className="sidebar-footer">
          <button
            type="button"
            className="sidebar-footer-btn"
            onClick={() => api.openFolder()}
          >
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>
            </svg>
            <span>Open Folder</span>
          </button>
        </div>
      </aside>

      {/* Main Content Pane */}
      <main className="app-main">
        {navTab === "gallery" && (
          <>
            {/* Top Toolbar */}
            <header className="main-topbar">
              <div className="topbar-tabs">
                <button
                  type="button"
                  className={`tab-btn ${canvasFilter === "all" ? "active" : ""}`}
                  onClick={() => setCanvasFilter("all")}
                >
                  Local files
                </button>
                <button
                  type="button"
                  className={`tab-btn ${canvasFilter === "today" ? "active" : ""}`}
                  onClick={() => setCanvasFilter("today")}
                >
                  Today ({groupedHistory.today.length})
                </button>
              </div>

              <div className="topbar-right">
                <div className="search-input-wrap">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                    <circle cx="11" cy="11" r="8"/>
                    <line x1="21" y1="21" x2="16.65" y2="16.65"/>
                  </svg>
                  <input
                    type="text"
                    className="search-input"
                    placeholder="Search captures…"
                    value={searchQuery}
                    onChange={(e) => setSearchQuery(e.target.value)}
                  />
                </div>

                <div className="view-toggle-group">
                  <button
                    type="button"
                    className={`view-toggle-btn ${viewMode === "grid" ? "active" : ""}`}
                    onClick={() => setViewMode("grid")}
                    title="Grid view"
                  >
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round">
                      <rect x="3" y="3" width="7" height="7"/>
                      <rect x="14" y="3" width="7" height="7"/>
                      <rect x="14" y="14" width="7" height="7"/>
                      <rect x="3" y="14" width="7" height="7"/>
                    </svg>
                  </button>
                  <button
                    type="button"
                    className={`view-toggle-btn ${viewMode === "list" ? "active" : ""}`}
                    onClick={() => setViewMode("list")}
                    title="List view"
                  >
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round">
                      <line x1="8" y1="6" x2="21" y2="6"/>
                      <line x1="8" y1="12" x2="21" y2="12"/>
                      <line x1="8" y1="18" x2="21" y2="18"/>
                      <line x1="3" y1="6" x2="3.01" y2="6"/>
                      <line x1="3" y1="12" x2="3.01" y2="12"/>
                      <line x1="3" y1="18" x2="3.01" y2="18"/>
                    </svg>
                  </button>
                </div>

                <button
                  type="button"
                  className={`btn-icon${refreshing ? " spinning" : ""}`}
                  onClick={refreshHistory}
                  title="Refresh list"
                >
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                    <polyline points="23 4 23 10 17 10"/>
                    <polyline points="1 20 1 14 7 14"/>
                    <path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"/>
                  </svg>
                </button>

                {/* 3-Dot More Options Dropdown */}
                <div className="more-menu-wrapper" ref={moreMenuRef}>
                  <button
                    type="button"
                    className={`btn-icon ${showMoreMenu ? "active" : ""}`}
                    onClick={() => setShowMoreMenu((v) => !v)}
                    title="More options"
                  >
                    <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                      <circle cx="12" cy="5" r="1.2"/>
                      <circle cx="12" cy="12" r="1.2"/>
                      <circle cx="12" cy="19" r="1.2"/>
                    </svg>
                  </button>

                  {showMoreMenu && (
                    <div className="dropdown-menu-card">
                      <button
                        type="button"
                        className="dropdown-menu-item"
                        onClick={() => {
                          setShowMoreMenu(false);
                          api.openFolder();
                        }}
                      >
                        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                          <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>
                        </svg>
                        <span>Open folder</span>
                      </button>

                      <div className="dropdown-menu-sep" />

                      <button
                        type="button"
                        className="dropdown-menu-item danger"
                        disabled={history.length === 0}
                        onClick={async () => {
                          setShowMoreMenu(false);
                          if (
                            settings.confirm_delete &&
                            !window.confirm("Clear all capture history from SnapDrop?")
                          ) {
                            return;
                          }
                          await api.clearHistory();
                          refreshHistory();
                        }}
                      >
                        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                          <polyline points="3 6 5 6 21 6"/>
                          <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>
                        </svg>
                        <span>Clear all captures</span>
                      </button>
                    </div>
                  )}
                </div>
              </div>
            </header>

            {/* Canvas Body */}
            <div
              className="canvas-body"
              onMouseDown={(e) => {
                // Deselect when clicking any non-interactive part of the
                // canvas: gaps between rows, the timeline headers/lines, or
                // empty space below the list. Rows, cards and the selection
                // bar keep their own behavior.
                const t = e.target as HTMLElement;
                if (!t.closest(".screenshot-row, .screenshot-card, .selection-bar")) {
                  clearSelection();
                }
              }}
            >
              {selected.size > 0 && (
                <div className="selection-bar">
                  <span className="selection-count">{selected.size} selected</span>
                  <span className="selection-hint">
                    Drag any selected capture to share them all at once
                  </span>
                  <button
                    type="button"
                    className="btn-icon"
                    onClick={clearSelection}
                    title="Clear selection (Esc)"
                  >
                    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                      <line x1="18" y1="6" x2="6" y2="18" />
                      <line x1="6" y1="6" x2="18" y2="18" />
                    </svg>
                  </button>
                </div>
              )}
              {filteredHistory.length === 0 ? (
                <div className="empty-state">
                  <svg className="empty-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
                    <rect x="3" y="3" width="18" height="18" rx="2" ry="2"/>
                    <circle cx="8.5" cy="8.5" r="1.5"/>
                    <polyline points="21 15 16 10 5 21"/>
                  </svg>
                  <h3>No Screenshots Found</h3>
                  <p>Capture your first screenshot by pressing {settings.hotkey || "Ctrl+Alt+S"} or clicking Screenshot in the sidebar.</p>
                  <button type="button" className="primary" onClick={() => api.captureNow()}>
                    Capture Now
                  </button>
                </div>
              ) : viewMode === "list" ? (
                <div>
                  {groupedHistory.today.length > 0 && (
                    <div className="timeline-group">
                      <div className="timeline-header">
                        <span className="timeline-title">Today</span>
                        <div className="timeline-line" />
                      </div>
                      <div className="screenshot-list">
                        {groupedHistory.today.map((item) => (
                          <div
                            key={item.path}
                            className={`screenshot-row${selected.has(item.path) ? " selected" : ""}`}
                            onMouseDown={(e) => handleRowMouseDown(e, item)}
                            title="Drag to drop into another app · Ctrl+click to select multiple"
                          >
                            <div className="screenshot-thumb-box">
                              {selected.has(item.path) && (
                                <span className="selection-check">
                                  <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="3.5" strokeLinecap="round" strokeLinejoin="round">
                                    <polyline points="20 6 9 17 4 12" />
                                  </svg>
                                </span>
                              )}
                              <PreviewThumb
                                item={item}
                                preview={previews[item.path]}
                                className="screenshot-thumb-img"
                              />
                            </div>

                            <div className="screenshot-main-info">
                              <span className="screenshot-name">{fileName(item.path)}</span>
                              <span className="screenshot-date">{formatDate(item.captured_at)}</span>
                            </div>

                            <div className="screenshot-meta">
                              <span className="meta-resolution">
                                {isVideoEntry(item)
                                  ? formatDuration(item.duration_secs)
                                  : item.width && item.height
                                    ? `${item.width}×${item.height}`
                                    : "—"}
                              </span>
                              <span className="meta-size">{formatBytes(item.size_bytes)}</span>
                            </div>

                            <div className="screenshot-actions" onMouseDown={(e) => e.stopPropagation()}>
                              <button
                                type="button"
                                className="btn-icon"
                                onClick={() => api.openCapture(item.path)}
                                title="Open in image viewer"
                              >
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                                  <path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/>
                                  <circle cx="12" cy="12" r="3"/>
                                </svg>
                              </button>

                              <button
                                type="button"
                                className="btn-icon"
                                onClick={() => api.revealCapture(item.path)}
                                title="Show in folder"
                              >
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                                  <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>
                                </svg>
                              </button>

                              <button
                                type="button"
                                className="btn-icon"
                                onClick={() => api.copyCapture(item.path)}
                                title="Copy to clipboard"
                              >
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                                  <rect x="9" y="9" width="13" height="13" rx="2" ry="2"/>
                                  <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>
                                </svg>
                              </button>

                              <button
                                type="button"
                                className="btn-icon danger"
                                onClick={async () => {
                                  if (
                                    settings.confirm_delete &&
                                    !window.confirm(`Delete ${fileName(item.path)}?`)
                                  ) {
                                    return;
                                  }
                                  await api.deleteCapture(item.path);
                                  // Update local state immediately instead of a full
                                  // refresh (which re-fetches every preview) so
                                  // deleting several captures in a row stays fast.
                                  setHistory((prev) => prev.filter((e) => e.path !== item.path));
                                  setPreviews((prev) => {
                                    const next = { ...prev };
                                    delete next[item.path];
                                    return next;
                                  });
                                  setSelected((prev) => {
                                    const next = new Set(prev);
                                    next.delete(item.path);
                                    return next;
                                  });
                                }}
                                title="Delete capture"
                              >
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                                  <polyline points="3 6 5 6 21 6"/>
                                  <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>
                                </svg>
                              </button>
                            </div>
                          </div>
                        ))}
                      </div>
                    </div>
                  )}

                  {groupedHistory.earlier.length > 0 && (
                    <div className="timeline-group">
                      <div className="timeline-header">
                        <span className="timeline-title">Earlier</span>
                        <div className="timeline-line" />
                      </div>
                      <div className="screenshot-list">
                        {groupedHistory.earlier.map((item) => (
                          <div
                            key={item.path}
                            className={`screenshot-row${selected.has(item.path) ? " selected" : ""}`}
                            onMouseDown={(e) => handleRowMouseDown(e, item)}
                            title="Drag to drop into another app · Ctrl+click to select multiple"
                          >
                            <div className="screenshot-thumb-box">
                              {selected.has(item.path) && (
                                <span className="selection-check">
                                  <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" strokeWidth="3.5" strokeLinecap="round" strokeLinejoin="round">
                                    <polyline points="20 6 9 17 4 12" />
                                  </svg>
                                </span>
                              )}
                              <PreviewThumb
                                item={item}
                                preview={previews[item.path]}
                                className="screenshot-thumb-img"
                              />
                            </div>

                            <div className="screenshot-main-info">
                              <span className="screenshot-name">{fileName(item.path)}</span>
                              <span className="screenshot-date">{formatDate(item.captured_at)}</span>
                            </div>

                            <div className="screenshot-meta">
                              <span className="meta-resolution">
                                {isVideoEntry(item)
                                  ? formatDuration(item.duration_secs)
                                  : item.width && item.height
                                    ? `${item.width}×${item.height}`
                                    : "—"}
                              </span>
                              <span className="meta-size">{formatBytes(item.size_bytes)}</span>
                            </div>

                            <div className="screenshot-actions" onMouseDown={(e) => e.stopPropagation()}>
                              <button
                                type="button"
                                className="btn-icon"
                                onClick={() => api.openCapture(item.path)}
                                title="Open in image viewer"
                              >
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                                  <path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/>
                                  <circle cx="12" cy="12" r="3"/>
                                </svg>
                              </button>

                              <button
                                type="button"
                                className="btn-icon"
                                onClick={() => api.revealCapture(item.path)}
                                title="Show in folder"
                              >
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                                  <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>
                                </svg>
                              </button>

                              <button
                                type="button"
                                className="btn-icon"
                                onClick={() => api.copyCapture(item.path)}
                                title="Copy to clipboard"
                              >
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                                  <rect x="9" y="9" width="13" height="13" rx="2" ry="2"/>
                                  <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>
                                </svg>
                              </button>

                              <button
                                type="button"
                                className="btn-icon danger"
                                onClick={async () => {
                                  if (
                                    settings.confirm_delete &&
                                    !window.confirm(`Delete ${fileName(item.path)}?`)
                                  ) {
                                    return;
                                  }
                                  await api.deleteCapture(item.path);
                                  // Update local state immediately instead of a full
                                  // refresh (which re-fetches every preview) so
                                  // deleting several captures in a row stays fast.
                                  setHistory((prev) => prev.filter((e) => e.path !== item.path));
                                  setPreviews((prev) => {
                                    const next = { ...prev };
                                    delete next[item.path];
                                    return next;
                                  });
                                  setSelected((prev) => {
                                    const next = new Set(prev);
                                    next.delete(item.path);
                                    return next;
                                  });
                                }}
                                title="Delete capture"
                              >
                                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                                  <polyline points="3 6 5 6 21 6"/>
                                  <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>
                                </svg>
                              </button>
                            </div>
                          </div>
                        ))}
                      </div>
                    </div>
                  )}
                </div>
              ) : (
                /* Card Grid View */
                <div className="screenshot-grid">
                  {filteredHistory.map((item) => (
                    <div
                      key={item.path}
                      className={`screenshot-card${selected.has(item.path) ? " selected" : ""}`}
                      onMouseDown={(e) => handleRowMouseDown(e, item)}
                      title="Drag to drop into another app · Ctrl+click to select multiple"
                    >
                      <div className="card-preview-box">
                        {selected.has(item.path) && (
                          <span className="selection-check">
                            <svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="3.5" strokeLinecap="round" strokeLinejoin="round">
                              <polyline points="20 6 9 17 4 12" />
                            </svg>
                          </span>
                        )}
                        <PreviewThumb
                          item={item}
                          preview={previews[item.path]}
                          className="card-preview-img"
                        />
                        <div
                          className="card-overlay"
                          onMouseDown={(e) => {
                            // The overlay covers the whole preview, so only
                            // presses on the action buttons should be
                            // swallowed — anything else falls through to the
                            // card so Ctrl/Shift+click selects and plain
                            // presses drag.
                            if ((e.target as HTMLElement).closest("button")) {
                              e.stopPropagation();
                            }
                          }}
                        >
                          <button
                            type="button"
                            className="card-overlay-btn"
                            onClick={() => api.openCapture(item.path)}
                            title="Open"
                          >
                            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                              <path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/>
                              <circle cx="12" cy="12" r="3"/>
                            </svg>
                          </button>
                          <button
                            type="button"
                            className="card-overlay-btn"
                            onClick={() => api.revealCapture(item.path)}
                            title="Show in folder"
                          >
                            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                              <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>
                            </svg>
                          </button>
                        </div>
                      </div>
                      <div className="card-body">
                        <span className="card-title" title={fileName(item.path)}>
                          {fileName(item.path)}
                        </span>
                        <div className="card-meta-row">
                          <span>
                            {isVideoEntry(item)
                              ? formatDuration(item.duration_secs)
                              : item.width && item.height
                                ? `${item.width}×${item.height}`
                                : "PNG"}
                          </span>
                          <span>{formatBytes(item.size_bytes)}</span>
                        </div>
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </div>
          </>
        )}

        {navTab === "settings" && (
          <>
            <header className="main-topbar">
              <div className="topbar-tabs">
                <span style={{ fontWeight: 600, fontSize: 14 }}>Preferences</span>
              </div>
              <div className="topbar-right">
                <button type="button" onClick={() => api.openDebugLog()}>
                  Open diagnostic log
                </button>
                <button
                  type="button"
                  className="primary"
                  onClick={save}
                  disabled={saving}
                >
                  {saving ? "Saving…" : "Save Settings"}
                </button>
              </div>
            </header>

            <div className="canvas-body">
              <div className="settings-canvas-wrap">
                {status && (
                  <div className={`status-alert ${status.kind}`}>
                    {status.text}
                  </div>
                )}

                <section className="settings-section">
                  <h2>Diagnostics</h2>
                  <p className="field-hint">Use this after the floating thumbnail stops responding. The log records renderer heartbeats, native window state, recovery attempts, and JavaScript errors.</p>
                  <div style={{ display: "flex", gap: 8, marginTop: 10 }}>
                    <button type="button" onClick={async () => setDebugLog(await api.getDebugLog())}>View diagnostic log</button>
                    <button type="button" onClick={() => api.openDebugLog()}>Open log in Notepad</button>
                  </div>
                  {debugLog !== null && (
                    <textarea readOnly value={debugLog} style={{ width: "100%", minHeight: 220, marginTop: 10, fontFamily: "monospace", fontSize: 11 }} />
                  )}
                </section>

                <section className="settings-section">
                  <h2>Capture</h2>
                  <div className="field">
                    <span className="field-label">Global Capture Hotkey</span>
                    <span className="field-hint">Works globally even while SnapDrop is minimized.</span>
                    <div style={{ marginTop: 6 }}>
                      <HotkeyField value={settings.hotkey} onChange={(v) => set({ hotkey: v })} />
                    </div>
                  </div>

                  <div className="field">
                    <span className="field-label">Text Capture Hotkey (OCR)</span>
                    <span className="field-hint">Select a region and the recognized text is copied to your clipboard. Offline — nothing leaves your PC.</span>
                    <div style={{ marginTop: 6 }}>
                      <HotkeyField value={settings.ocr_hotkey} onChange={(v) => set({ ocr_hotkey: v })} />
                    </div>
                  </div>

                  <div className="field">
                    <span className="field-label">Last Area Hotkey</span>
                    <span className="field-hint">Re-opens the selection on the last captured area — click to capture instantly, drag to move or resize.</span>
                    <div style={{ marginTop: 6 }}>
                      <HotkeyField value={settings.last_area_hotkey} onChange={(v) => set({ last_area_hotkey: v })} />
                    </div>
                  </div>

                  <div className="field">
                    <span className="field-label">Video Recording Hotkey</span>
                    <span className="field-hint">Select a region on screen and start recording it to MP4 immediately. Stop via the tray icon.</span>
                    <div style={{ marginTop: 6 }}>
                      <HotkeyField value={settings.video_hotkey} onChange={(v) => set({ video_hotkey: v })} />
                    </div>
                  </div>

                  <div className="field">
                    <span className="field-label">Video Frame Rate</span>
                    <span className="field-hint">Frames per second. 60 is smoother for fast motion (video, scrolling); 30 uses less CPU and makes smaller files.</span>
                    <select
                      style={{ marginTop: 6 }}
                      value={String(settings.video_fps ?? 60)}
                      onChange={(e) => set({ video_fps: Number(e.target.value) })}
                    >
                      <option value="30">30 fps</option>
                      <option value="60">60 fps</option>
                    </select>
                  </div>

                  <div className="field">
                    <span className="field-label">Screenshot Directory</span>
                    <div style={{ display: "flex", gap: 8, marginTop: 4 }}>
                      <input
                        type="text"
                        style={{ flex: 1 }}
                        value={settings.screenshot_dir}
                        onChange={(e) => set({ screenshot_dir: e.target.value })}
                        spellCheck={false}
                      />
                      <button
                        type="button"
                        onClick={async () => {
                          const picked = await api.pickFolder(settings.screenshot_dir);
                          if (picked) set({ screenshot_dir: picked });
                        }}
                      >
                        Browse…
                      </button>
                    </div>
                  </div>

                  <div className="field-row" style={{ marginTop: 12 }}>
                    <div>
                      <span className="field-label">Start with Windows</span>
                      <div className="field-hint">Launch SnapDrop automatically on system startup.</div>
                    </div>
                    <input
                      type="checkbox"
                      checked={settings.start_with_windows}
                      onChange={(e) => set({ start_with_windows: e.target.checked })}
                    />
                  </div>

                  <div className="field-row" style={{ marginTop: 12 }}>
                    <div>
                      <span className="field-label">Annotate before showing thumbnail</span>
                      <div className="field-hint">Hold Ctrl while selecting to do the opposite: skip the editor when this is on, open it when off.</div>
                    </div>
                    <input
                      type="checkbox"
                      checked={settings.show_editor_after_capture}
                      onChange={(e) => set({ show_editor_after_capture: e.target.checked })}
                    />
                  </div>

                  <div className="field" style={{ marginTop: 12 }}>
                    <span className="field-label">Shift+select delay (seconds)</span>
                    <span className="field-hint">Hold Shift while selecting to capture after this delay — open menus and tooltips before the shot fires. 0 = Shift does nothing (instant capture).</span>
                    <input
                      type="number"
                      style={{ width: 140, marginTop: 4 }}
                      min={0}
                      max={60}
                      value={settings.capture_delay_secs}
                      onChange={(e) =>
                        set({ capture_delay_secs: Math.max(0, Math.min(60, Number(e.target.value) || 0)) })
                      }
                    />
                  </div>
                </section>

                <section className="settings-section">
                  <h2>Keyboard Shortcuts</h2>
                  <div className="shortcut-list">
                    <div className="shortcut-item">
                      <span className="shortcut-label">Capture screenshot</span>
                      <kbd className="shortcut-keys">{settings.hotkey || "Ctrl+Alt+S"}</kbd>
                    </div>
                    <div className="shortcut-item">
                      <span className="shortcut-label">Capture text to clipboard (OCR)</span>
                      <kbd className="shortcut-keys">{settings.ocr_hotkey || "Ctrl+Alt+O"}</kbd>
                    </div>
                    <div className="shortcut-item">
                      <span className="shortcut-label">While selecting: flip annotation editor</span>
                      <kbd className="shortcut-keys">Hold Ctrl</kbd>
                    </div>
                    <div className="shortcut-item">
                      <span className="shortcut-label">While selecting: delayed capture (fire after countdown)</span>
                      <kbd className="shortcut-keys">Hold Shift</kbd>
                    </div>
                    <div className="shortcut-item">
                      <span className="shortcut-label">Re-capture the last area (click the box to capture instantly)</span>
                      <kbd className="shortcut-keys">{settings.last_area_hotkey || "Ctrl+Alt+L"}</kbd>
                    </div>
                    <div className="shortcut-item">
                      <span className="shortcut-label">Select a region for video recording (toolbar: Rec to start)</span>
                      <kbd className="shortcut-keys">{settings.video_hotkey || "Ctrl+Alt+V"}</kbd>
                    </div>
                    <div className="shortcut-item">
                      <span className="shortcut-label">Stop video recording (while recording)</span>
                      <kbd className="shortcut-keys">Toolbar Stop or Tray</kbd>
                    </div>
                    <div className="shortcut-item">
                      <span className="shortcut-label">Dismiss floating thumbnail or cancel</span>
                      <kbd className="shortcut-keys">Esc</kbd>
                    </div>
                    <div className="shortcut-item">
                      <span className="shortcut-label">Editor: confirm and finish</span>
                      <kbd className="shortcut-keys">Enter</kbd>
                    </div>
                    <div className="shortcut-item">
                      <span className="shortcut-label">Editor: undo last stroke</span>
                      <kbd className="shortcut-keys">Ctrl+Z</kbd>
                    </div>
                  </div>
                </section>

                <section className="settings-section">
                  <h2>Floating Thumbnail</h2>
                  <div className="field-row">
                    <div>
                      <span className="field-label">Show floating thumbnail</span>
                      <div className="field-hint">Display draggable desktop pill upon capture.</div>
                    </div>
                    <input
                      type="checkbox"
                      checked={settings.show_thumbnail}
                      onChange={(e) => set({ show_thumbnail: e.target.checked })}
                    />
                  </div>

                  <div className="field" style={{ marginTop: 12 }}>
                    <span className="field-label">Auto-dismiss Duration (seconds)</span>
                    <span className="field-hint">0 = keep thumbnail until clicked or dragged.</span>
                    <input
                      type="number"
                      style={{ width: 140, marginTop: 4 }}
                      min={0}
                      max={3600}
                      value={settings.thumbnail_duration_secs}
                      onChange={(e) =>
                        set({ thumbnail_duration_secs: Math.max(0, Number(e.target.value) || 0) })
                      }
                    />
                  </div>

                  <div className="field" style={{ marginTop: 12 }}>
                    <span className="field-label">Thumbnail Position</span>
                    <select
                      style={{ width: 180, marginTop: 4 }}
                      value={settings.thumbnail_position}
                      onChange={(e) => set({ thumbnail_position: e.target.value })}
                    >
                      <option value="bottom_left">Bottom-Left</option>
                      <option value="bottom_right">Bottom-Right</option>
                      <option value="top_left">Top-Left</option>
                      <option value="top_right">Top-Right</option>
                    </select>
                  </div>
                </section>

                <section className="settings-section">
                  <h2>Behavior & History</h2>
                  <div className="field-row">
                    <div>
                      <span className="field-label">Copy screenshot to clipboard</span>
                      <div className="field-hint">Automatic Ctrl+V paste support for captured images.</div>
                    </div>
                    <input
                      type="checkbox"
                      checked={settings.copy_to_clipboard}
                      onChange={(e) => set({ copy_to_clipboard: e.target.checked })}
                    />
                  </div>

                  <div className="field-row" style={{ marginTop: 12 }}>
                    <div>
                      <span className="field-label">Keep file after drag-to-drop</span>
                      <div className="field-hint">When disabled, successful move-drops remove the file.</div>
                    </div>
                    <input
                      type="checkbox"
                      checked={settings.keep_after_drag}
                      onChange={(e) => set({ keep_after_drag: e.target.checked })}
                    />
                  </div>

                  <div className="field-row" style={{ marginTop: 12 }}>
                    <div>
                      <span className="field-label">Confirm before deleting captures</span>
                    </div>
                    <input
                      type="checkbox"
                      checked={settings.confirm_delete}
                      onChange={(e) => set({ confirm_delete: e.target.checked })}
                    />
                  </div>

                  <div className="field-row" style={{ marginTop: 12 }}>
                    <div>
                      <span className="field-label">Close button minimizes to tray</span>
                      <div className="field-hint">When off, pressing X quits the app.</div>
                    </div>
                    <input
                      type="checkbox"
                      checked={settings.close_to_tray}
                      onChange={(e) => set({ close_to_tray: e.target.checked })}
                    />
                  </div>
                </section>
              </div>
            </div>
          </>
        )}

        {navTab === "about" && (
          <>
            <header className="main-topbar">
              <div className="topbar-tabs">
                <span style={{ fontWeight: 600, fontSize: 14 }}>About SnapDrop</span>
              </div>
            </header>

            <div className="canvas-body">
              <div className="about-card">
                <div className="about-logo-large">
                  <img src="/assets/SnapDrop_Square.png" alt="SnapDrop" className="about-logo-img" />
                </div>
                <h2>SnapDrop</h2>
                <p>Version {version || "1.0.0"} • Ultra-fast Windows screen capture & real-time drag-and-drop utility.</p>
                <div className="about-features-list">
                  <div className="about-feature-item">
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12"/></svg>
                    <span><strong>100% Local-First:</strong> Screenshots never leave your device.</span>
                  </div>
                  <div className="about-feature-item">
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12"/></svg>
                    <span><strong>Native COM Drag-and-Drop:</strong> Drop straight into Discord, Slack, browsers.</span>
                  </div>
                  <div className="about-feature-item">
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12"/></svg>
                    <span><strong>Multi-monitor DPI aware:</strong> Physical-pixel precision selection.</span>
                  </div>
                </div>
              </div>
            </div>
          </>
        )}
      </main>
    </div>
  );
}
