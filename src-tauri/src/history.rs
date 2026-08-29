//! Lightweight recent-capture history (file-path references only).

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::settings;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub path: String,
    pub captured_at: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// "image" or "video" — old entries without the field default to image.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Video duration in whole seconds (0 for images).
    #[serde(default)]
    pub duration_secs: u64,
}

fn default_kind() -> String {
    "image".into()
}

pub struct HistoryState(pub Mutex<Vec<HistoryEntry>>);

pub fn history_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("history.json")
}

pub fn enrich_entry_metadata(entry: &mut HistoryEntry) {
    if let Ok(meta) = fs::metadata(&entry.path) {
        entry.size_bytes = meta.len();
    }
    if entry.width == 0 || entry.height == 0 {
        if let Ok(reader) = image::ImageReader::open(&entry.path) {
            if let Ok(reader) = reader.with_guessed_format() {
                if let Ok(dimensions) = reader.into_dimensions() {
                    entry.width = dimensions.0;
                    entry.height = dimensions.1;
                }
            }
        }
    }
}

/// Remove entries whose files no longer exist on disk. Returns true if any
/// entry was removed. Keeps the gallery/tray in sync with the folder even
/// when files are deleted outside the app.
pub fn prune_missing(entries: &mut Vec<HistoryEntry>) -> bool {
    let before = entries.len();
    entries.retain(|e| fs::metadata(&e.path).is_ok());
    entries.len() != before
}

pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let mut entries = load(app).unwrap_or_default();
    prune_missing(&mut entries);
    for entry in &mut entries {
        enrich_entry_metadata(entry);
    }
    app.manage(HistoryState(Mutex::new(entries)));
    Ok(())
}

fn load(app: &AppHandle) -> Option<Vec<HistoryEntry>> {
    let raw = fs::read_to_string(history_path(app)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn persist(app: &AppHandle, entries: &[HistoryEntry]) {
    let path = history_path(app);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(raw) = serde_json::to_string_pretty(entries) {
        let _ = fs::write(path, raw);
    }
}

pub fn entries(app: &AppHandle) -> Vec<HistoryEntry> {
    if let Some(state) = app.try_state::<HistoryState>() {
        // Snapshot under the lock, then persist + emit AFTER the lock is
        // released (the event handler re-enters history via get_history).
        let (result, pruned) = {
            let mut inner = state.inner().0.lock().unwrap();
            // Prune entries whose files were deleted outside the app (e.g.
            // from Explorer). Without this the gallery/tray would show stale
            // captures until restart.
            let pruned = prune_missing(&mut inner);
            for entry in inner.iter_mut() {
                if entry.size_bytes == 0 || entry.width == 0 {
                    enrich_entry_metadata(entry);
                }
            }
            (inner.clone(), pruned)
        };
        if pruned {
            persist(app, &result);
            let _ = app.emit("history-updated", ());
        }
        result
    } else {
        let mut entries = load(app).unwrap_or_default();
        prune_missing(&mut entries);
        entries
    }
}

/// Add a capture at the front of the stack, trimmed to the configured max.
pub fn add(app: &AppHandle, path: String, captured_at: String) {
    let mut new_entry = HistoryEntry {
        path,
        captured_at,
        size_bytes: 0,
        width: 0,
        height: 0,
        kind: "image".into(),
        duration_secs: 0,
    };
    enrich_entry_metadata(&mut new_entry);
    insert_entry(app, new_entry);
}

/// Add a finished video recording (an MP4) to the history. `width`/`height`
/// are the recorded region's dimensions; `duration_secs` comes from the
/// recorder's elapsed time.
pub fn add_video(
    app: &AppHandle,
    path: String,
    captured_at: String,
    duration_secs: u64,
    width: u32,
    height: u32,
) {
    let mut new_entry = HistoryEntry {
        path,
        captured_at,
        size_bytes: 0,
        width,
        height,
        kind: "video".into(),
        duration_secs,
    };
    enrich_entry_metadata(&mut new_entry);
    insert_entry(app, new_entry);
}

fn insert_entry(app: &AppHandle, new_entry: HistoryEntry) {
    let max = settings::get(app).max_history.max(1);
    let inserted = if let Some(state) = app.try_state::<HistoryState>() {
        let mut inner = state.inner().0.lock().unwrap();
        inner.retain(|e| e.path != new_entry.path);
        inner.insert(0, new_entry);
        inner.truncate(max);
        let snapshot = inner.clone();
        persist(app, &snapshot);
        true
    } else {
        false
    };
    // Emit only AFTER the state lock is released: the event handler on the
    // main thread re-enters history via the get_history command, and emitting
    // while holding the lock risks a lock-order stall.
    if inserted {
        let _ = app.emit("history-updated", ());
    }
}

pub fn remove(app: &AppHandle, path: &str) -> bool {
    if let Some(state) = app.try_state::<HistoryState>() {
        let (changed, snapshot) = {
            let mut inner = state.inner().0.lock().unwrap();
            let before = inner.len();
            inner.retain(|e| e.path != path);
            let changed = inner.len() != before;
            (changed, inner.clone())
        };
        if changed {
            persist(app, &snapshot);
            let _ = app.emit("history-updated", ());
        }
        changed
    } else {
        false
    }
}

pub fn clear(app: &AppHandle) {
    if let Some(state) = app.try_state::<HistoryState>() {
        {
            let mut inner = state.inner().0.lock().unwrap();
            inner.clear();
        }
        persist(app, &[]);
        let _ = app.emit("history-updated", ());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        let e = HistoryEntry {
            path: r"C:\Pictures\SnapDrop\SnapDrop_2026-08-24_145423.png".into(),
            captured_at: "2026-08-24T14:54:23".into(),
            size_bytes: 1024,
            width: 1920,
            height: 1080,
            kind: "video".into(),
            duration_secs: 12,
        };
        let raw = serde_json::to_string(&e).unwrap();
        let back: HistoryEntry = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.path, e.path);
    }

    #[test]
    fn prune_missing_removes_deleted_files() {
        let dir = std::env::temp_dir().join(format!("snapdrop-history-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("exists.png");
        std::fs::write(&existing, b"png").unwrap();
        let missing = dir.join("missing.png");

        let mut entries = vec![
            HistoryEntry {
                path: existing.to_string_lossy().into_owned(),
                captured_at: "2026-08-28T10:00:00".into(),
                size_bytes: 0,
                width: 0,
                height: 0,
                kind: "image".into(),
                duration_secs: 0,
            },
            HistoryEntry {
                path: missing.to_string_lossy().into_owned(),
                captured_at: "2026-08-28T10:00:00".into(),
                size_bytes: 0,
                width: 0,
                height: 0,
                kind: "image".into(),
                duration_secs: 0,
            },
        ];

        assert!(prune_missing(&mut entries));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, existing.to_string_lossy());

        // Second pass finds nothing to remove.
        assert!(!prune_missing(&mut entries));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
