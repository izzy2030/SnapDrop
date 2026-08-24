import { useCallback, useEffect, useState } from "react";
import { api, HistoryEntry, Settings } from "./api";

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

function Field({
  label,
  children,
  hint,
}: {
  label: string;
  children: React.ReactNode;
  hint?: string;
}) {
  return (
    <label className="field">
      <span className="field-label">{label}</span>
      {children}
      {hint && <span className="field-hint">{hint}</span>}
    </label>
  );
}

export default function SettingsApp() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [status, setStatus] = useState<Status>(null);
  const [saving, setSaving] = useState(false);
  const [version, setVersion] = useState("");

  const refreshHistory = useCallback(() => {
    api.getHistory().then(setHistory).catch(() => {});
  }, []);

  useEffect(() => {
    api.getSettings().then(setSettings).catch(() => {});
    api.getAppVersion().then(setVersion).catch(() => {});
    refreshHistory();
  }, [refreshHistory]);

  const set = (patch: Partial<Settings>) =>
    setSettings((s) => (s ? { ...s, ...patch } : s));

  const save = async () => {
    if (!settings) return;
    setSaving(true);
    setStatus(null);
    try {
      await api.updateSettings(settings);
      setStatus({ kind: "info", text: "Settings saved." });
    } catch (e) {
      // Hotkey conflict or invalid folder → keep old settings, show error.
      setStatus({ kind: "error", text: String(e) });
      api.getSettings().then(setSettings).catch(() => {});
    } finally {
      setSaving(false);
    }
  };

  if (!settings) {
    return <div className="settings-loading">Loading…</div>;
  }

  const fileName = (p: string) => p.split(/[\\/]/).pop() ?? p;

  return (
    <div className="settings">
      <header className="settings-header">
        <h1>SnapDrop</h1>
        <div className="settings-header-actions">
          <button type="button" onClick={() => api.captureNow()}>
            Capture Now
          </button>
          <button type="button" className="primary" onClick={save} disabled={saving}>
            {saving ? "Saving…" : "Save Settings"}
          </button>
        </div>
      </header>

      {status && <div className={`status ${status.kind}`}>{status.text}</div>}

      <section>
        <h2>Capture</h2>
        <Field label="Global hotkey" hint="Works while SnapDrop runs in the background.">
          <HotkeyField value={settings.hotkey} onChange={(v) => set({ hotkey: v })} />
        </Field>
        <Field label="Screenshot folder">
          <div className="row">
            <input
              type="text"
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
        </Field>
        <Field label="Image format">
          <select
            value={settings.format}
            onChange={(e) => set({ format: e.target.value })}
          >
            <option value="png">PNG</option>
          </select>
        </Field>
        <Field label="Start with Windows">
          <input
            type="checkbox"
            checked={settings.start_with_windows}
            onChange={(e) => set({ start_with_windows: e.target.checked })}
          />
        </Field>
        <Field
          label="Annotate before showing thumbnail"
          hint="Pause with the drawing toolbar after capture; Enter confirms, Esc skips."
        >
          <input
            type="checkbox"
            checked={settings.show_editor_after_capture}
            onChange={(e) => set({ show_editor_after_capture: e.target.checked })}
          />
        </Field>
      </section>

      <section>
        <h2>Thumbnail</h2>
        <Field label="Show floating thumbnail">
          <input
            type="checkbox"
            checked={settings.show_thumbnail}
            onChange={(e) => set({ show_thumbnail: e.target.checked })}
          />
        </Field>
        <Field label="Auto-dismiss after (seconds)" hint="0 = keep until closed or dragged.">
          <input
            type="number"
            min={0}
            max={3600}
            value={settings.thumbnail_duration_secs}
            onChange={(e) =>
              set({ thumbnail_duration_secs: Math.max(0, Number(e.target.value) || 0) })
            }
          />
        </Field>
        <Field label="Size">
          <select
            value={settings.thumbnail_size}
            onChange={(e) => set({ thumbnail_size: e.target.value })}
          >
            <option value="small">Small</option>
            <option value="medium">Medium</option>
            <option value="large">Large</option>
          </select>
        </Field>
        <Field label="Position">
          <select
            value={settings.thumbnail_position}
            onChange={(e) => set({ thumbnail_position: e.target.value })}
          >
            <option value="bottom_left">Bottom-left</option>
            <option value="bottom_right">Bottom-right</option>
            <option value="top_left">Top-left</option>
            <option value="top_right">Top-right</option>
          </select>
        </Field>
      </section>

      <section>
        <h2>Behavior</h2>
        <Field label="Copy screenshot to clipboard">
          <input
            type="checkbox"
            checked={settings.copy_to_clipboard}
            onChange={(e) => set({ copy_to_clipboard: e.target.checked })}
          />
        </Field>
        <Field label="Keep screenshot after drag" hint="When off, a successful move-drop deletes the file.">
          <input
            type="checkbox"
            checked={settings.keep_after_drag}
            onChange={(e) => set({ keep_after_drag: e.target.checked })}
          />
        </Field>
        <Field label="Hide thumbnail after successful drop" hint="Removes the thumbnail as soon as the file lands in another app.">
          <input
            type="checkbox"
            checked={settings.hide_after_drop}
            onChange={(e) => set({ hide_after_drop: e.target.checked })}
          />
        </Field>
        <Field label="Maximum recent screenshots">
          <input
            type="number"
            min={1}
            max={100}
            value={settings.max_history}
            onChange={(e) =>
              set({ max_history: Math.max(1, Math.min(100, Number(e.target.value) || 10)) })
            }
          />
        </Field>
        <Field label="Confirm before deleting captures">
          <input
            type="checkbox"
            checked={settings.confirm_delete}
            onChange={(e) => set({ confirm_delete: e.target.checked })}
          />
        </Field>
      </section>

      <section>
        <h2>
          Recent Captures
          <button
            type="button"
            className="link danger"
            disabled={history.length === 0}
            onClick={async () => {
              await api.clearHistory();
              refreshHistory();
            }}
          >
            Clear all
          </button>
        </h2>
        {history.length === 0 ? (
          <p className="muted">No captures yet — press {settings.hotkey}.</p>
        ) : (
          <ul className="history-list">
            {history.map((h) => (
              <li key={h.path}>
                <span className="history-name" title={h.path}>
                  {fileName(h.path)}
                </span>
                <span className="history-time">{h.captured_at.slice(0, 19).replace("T", " ")}</span>
                <span className="history-actions">
                  <button type="button" onClick={() => api.openCapture(h.path)}>
                    Open
                  </button>
                  <button type="button" onClick={() => api.revealCapture(h.path)}>
                    Show in folder
                  </button>
                  <button
                    type="button"
                    className="danger"
                    onClick={async () => {
                      if (
                        settings.confirm_delete &&
                        !window.confirm(`Delete ${fileName(h.path)}?`)
                      ) {
                        return;
                      }
                      await api.deleteCapture(h.path);
                      refreshHistory();
                    }}
                  >
                    Delete
                  </button>
                </span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="about">
        <h2>About</h2>
        <p>
          SnapDrop <span className="muted">v{version || "1.0.0"}</span> — capture a region and
          drag the thumbnail straight into another app.
        </p>
        <p className="muted">
          Local-first: screenshots never leave your computer. No accounts, no cloud.
        </p>
      </section>
    </div>
  );
}
