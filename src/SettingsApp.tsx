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
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<Status>(null);
  const [version, setVersion] = useState<string>("");
  const [showMoreMenu, setShowMoreMenu] = useState(false);
  const moreMenuRef = useRef<HTMLDivElement>(null);

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

  const refreshHistory = useCallback(() => {
    api
      .getHistory()
      .then((h) => {
        setHistory(h);
        // Fetch previews for visible entries asynchronously
        h.forEach((entry) => {
          api
            .getCapturePreview(entry.path)
            .then((preview) => {
              setPreviews((prev) => {
                if (prev[entry.path] === preview) return prev;
                return { ...prev, [entry.path]: preview };
              });
            })
            .catch(() => {});
        });
      })
      .catch((e) => console.error("History fetch error:", e));
  }, []);

  useEffect(() => {
    api.getSettings().then(setSettings).catch((e) => setStatus({ kind: "error", text: String(e) }));
    refreshHistory();
    api.getAppVersion().then(setVersion).catch(() => {});

    const unCaptured = listen("thumbnail-captured", () => refreshHistory());
    const unHistory = listen("history-updated", () => refreshHistory());
    const onFocus = () => refreshHistory();
    window.addEventListener("focus", onFocus);

    return () => {
      unCaptured.then((f) => f());
      unHistory.then((f) => f());
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

  const handleDragStart = (e: React.MouseEvent, path: string) => {
    // Only trigger on direct left click hold/drag
    if (e.button !== 0) return;
    api.startDrag(path).then((outcome) => {
      if (outcome.moved) {
        refreshHistory();
      }
    }).catch(console.error);
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
            <span className="hero-shortcut">{settings.hotkey || "Ctrl+Shift+4"}</span>
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
                  className="btn-icon"
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
            <div className="canvas-body">
              {filteredHistory.length === 0 ? (
                <div className="empty-state">
                  <svg className="empty-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
                    <rect x="3" y="3" width="18" height="18" rx="2" ry="2"/>
                    <circle cx="8.5" cy="8.5" r="1.5"/>
                    <polyline points="21 15 16 10 5 21"/>
                  </svg>
                  <h3>No Screenshots Found</h3>
                  <p>Capture your first screenshot by pressing {settings.hotkey || "Ctrl+Shift+4"} or clicking Screenshot in the sidebar.</p>
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
                            className="screenshot-row"
                            onMouseDown={(e) => handleDragStart(e, item.path)}
                            title="Drag to drop into another app, or click actions"
                          >
                            <div className="screenshot-thumb-box">
                              {previews[item.path] ? (
                                <img
                                  src={previews[item.path]}
                                  alt={fileName(item.path)}
                                  className="screenshot-thumb-img"
                                />
                              ) : (
                                <span className="screenshot-thumb-placeholder">PNG</span>
                              )}
                            </div>

                            <div className="screenshot-main-info">
                              <span className="screenshot-name">{fileName(item.path)}</span>
                              <span className="screenshot-date">{formatDate(item.captured_at)}</span>
                            </div>

                            <div className="screenshot-meta">
                              <span className="meta-resolution">
                                {item.width && item.height ? `${item.width}×${item.height}` : "—"}
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
                                  refreshHistory();
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
                            className="screenshot-row"
                            onMouseDown={(e) => handleDragStart(e, item.path)}
                            title="Drag to drop into another app, or click actions"
                          >
                            <div className="screenshot-thumb-box">
                              {previews[item.path] ? (
                                <img
                                  src={previews[item.path]}
                                  alt={fileName(item.path)}
                                  className="screenshot-thumb-img"
                                />
                              ) : (
                                <span className="screenshot-thumb-placeholder">PNG</span>
                              )}
                            </div>

                            <div className="screenshot-main-info">
                              <span className="screenshot-name">{fileName(item.path)}</span>
                              <span className="screenshot-date">{formatDate(item.captured_at)}</span>
                            </div>

                            <div className="screenshot-meta">
                              <span className="meta-resolution">
                                {item.width && item.height ? `${item.width}×${item.height}` : "—"}
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
                                  refreshHistory();
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
                      className="screenshot-card"
                      onMouseDown={(e) => handleDragStart(e, item.path)}
                    >
                      <div className="card-preview-box">
                        {previews[item.path] ? (
                          <img
                            src={previews[item.path]}
                            alt={fileName(item.path)}
                            className="card-preview-img"
                          />
                        ) : (
                          <span className="screenshot-thumb-placeholder">PNG Preview</span>
                        )}
                        <div className="card-overlay" onMouseDown={(e) => e.stopPropagation()}>
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
                          <span>{item.width && item.height ? `${item.width}×${item.height}` : "PNG"}</span>
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
                  <h2>Capture</h2>
                  <div className="field">
                    <span className="field-label">Global Capture Hotkey</span>
                    <span className="field-hint">Works globally even while SnapDrop is minimized.</span>
                    <div style={{ marginTop: 6 }}>
                      <HotkeyField value={settings.hotkey} onChange={(v) => set({ hotkey: v })} />
                    </div>
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
                  <img src="/assets/logo-full.png" alt="SnapDrop" className="about-logo-img" />
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
