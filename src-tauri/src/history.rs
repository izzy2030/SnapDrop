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

pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let mut entries = load(app).unwrap_or_default();
    // Prune entries whose files no longer exist.
    entries.retain(|e| fs::metadata(&e.path).is_ok());
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
        let mut inner = state.inner().0.lock().unwrap();
        for entry in inner.iter_mut() {
            if entry.size_bytes == 0 || entry.width == 0 {
                enrich_entry_metadata(entry);
            }
        }
        inner.clone()
    } else {
        load(app).unwrap_or_default()
    }
}

/// Add a capture at the front of the stack, trimmed to the configured max.
pub fn add(app: &AppHandle, path: String, captured_at: String) {
    let max = settings::get(app).max_history.max(1);
    if let Some(state) = app.try_state::<HistoryState>() {
        let mut inner = state.inner().0.lock().unwrap();
        inner.retain(|e| e.path != path);
        let mut new_entry = HistoryEntry {
            path,
            captured_at,
            size_bytes: 0,
            width: 0,
            height: 0,
        };
        enrich_entry_metadata(&mut new_entry);
        inner.insert(0, new_entry);
        inner.truncate(max);
        let snapshot = inner.clone();
        persist(app, &snapshot);
        let _ = app.emit("history-updated", ());
    }
}

pub fn remove(app: &AppHandle, path: &str) -> bool {
    if let Some(state) = app.try_state::<HistoryState>() {
        let mut inner = state.inner().0.lock().unwrap();
        let before = inner.len();
        inner.retain(|e| e.path != path);
        let changed = inner.len() != before;
        if changed {
            let snapshot = inner.clone();
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
        let mut inner = state.inner().0.lock().unwrap();
        inner.clear();
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
        };
        let raw = serde_json::to_string(&e).unwrap();
        let back: HistoryEntry = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.path, e.path);
    }
}
