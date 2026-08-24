//! Lightweight recent-capture history (file-path references only).

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::settings;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub path: String,
    pub captured_at: String,
}

pub struct HistoryState(pub Mutex<Vec<HistoryEntry>>);

pub fn history_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("history.json")
}

pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let mut entries = load(app).unwrap_or_default();
    // Prune entries whose files no longer exist.
    entries.retain(|e| fs::metadata(&e.path).is_ok());
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
    let state = app.state::<HistoryState>();
    let inner = state.0.lock().unwrap();
    inner.clone()
}

/// Add a capture at the front of the stack, trimmed to the configured max.
pub fn add(app: &AppHandle, path: String, captured_at: String) {
    let max = settings::get(app).max_history.max(1);
    let state = app.state::<HistoryState>();
    let mut inner = state.0.lock().unwrap();
    inner.retain(|e| e.path != path);
    inner.insert(
        0,
        HistoryEntry {
            path,
            captured_at,
        },
    );
    inner.truncate(max);
    let snapshot = inner.clone();
    persist(app, &snapshot);
}

pub fn remove(app: &AppHandle, path: &str) -> bool {
    let state = app.state::<HistoryState>();
    let mut inner = state.0.lock().unwrap();
    let before = inner.len();
    inner.retain(|e| e.path != path);
    let changed = inner.len() != before;
    if changed {
        let snapshot = inner.clone();
        persist(app, &snapshot);
    }
    changed
}

pub fn clear(app: &AppHandle) {
    let state = app.state::<HistoryState>();
    let mut inner = state.0.lock().unwrap();
    inner.clear();
    persist(app, &[]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        let e = HistoryEntry {
            path: r"C:\Pictures\SnapDrop\SnapDrop_2026-08-24_145423.png".into(),
            captured_at: "2026-08-24T14:54:23".into(),
        };
        let raw = serde_json::to_string(&e).unwrap();
        let back: HistoryEntry = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.path, e.path);
    }
}
