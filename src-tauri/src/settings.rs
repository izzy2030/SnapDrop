//! Persistent settings (serde JSON in the app config dir).

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::filename;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub hotkey: String,
    pub screenshot_dir: String,
    pub format: String,
    pub start_with_windows: bool,
    pub show_thumbnail: bool,
    pub thumbnail_duration_secs: u32,
    pub thumbnail_size: String,
    pub thumbnail_position: String,
    pub copy_to_clipboard: bool,
    pub keep_after_drag: bool,
    /// Hide the thumbnail as soon as a drag is successfully dropped anywhere.
    pub hide_after_drop: bool,
    /// Pause after capture with the annotation editor before showing the thumbnail.
    pub show_editor_after_capture: bool,
    pub max_history: usize,
    pub confirm_delete: bool,
    /// Close button sends the app to the tray instead of quitting.
    pub close_to_tray: bool,
    /// Global hotkey: capture a region and copy the recognized text (OCR).
    pub ocr_hotkey: String,
    /// Seconds to wait after selecting the area before the shot fires.
    /// 0 = capture immediately on release. Delayed capture exists so the
    /// user can open menus/tooltips before the screenshot is taken.
    pub capture_delay_secs: u32,
    /// Global hotkey: re-open the selection overlay pre-positioned on the
    /// last captured region (click to capture again, drag to move/resize).
    pub last_area_hotkey: String,
    /// Global hotkey: select a region on screen and start video-recording it
    /// (Ctrl+Alt+V by default).
    pub video_hotkey: String,
    /// The most recently captured region (virtual-screen coords). Seeded by
    /// every image capture; None until the first screenshot.
    pub last_area: Option<LastArea>,
}

/// A previously captured region, in physical virtual-screen coordinates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastArea {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hotkey: "Ctrl+Shift+4".into(),
            screenshot_dir: filename::default_screenshot_dir().to_string_lossy().to_string(),
            format: "png".into(),
            start_with_windows: true,
            show_thumbnail: true,
            thumbnail_duration_secs: 0,
            thumbnail_size: "medium".into(),
            thumbnail_position: "bottom_left".into(),
            copy_to_clipboard: true,
            keep_after_drag: true,
            hide_after_drop: true,
            show_editor_after_capture: true,
            max_history: 10,
            confirm_delete: false,
            close_to_tray: true,
            ocr_hotkey: "Ctrl+Shift+5".into(),
            // Duration of the delayed capture, armed by holding Shift while
            // selecting. 0 = Shift does nothing (instant capture).
            capture_delay_secs: 3,
            last_area_hotkey: "Ctrl+Alt+4".into(),
            video_hotkey: "Ctrl+Alt+V".into(),
            last_area: None,
        }
    }
}

pub struct SettingsState(pub Mutex<Settings>);

pub fn settings_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("settings.json")
}

pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let settings = load(app).unwrap_or_default();
    app.manage(SettingsState(Mutex::new(settings.clone())));

    // Apply autostart per the stored preference.
    if settings.start_with_windows {
        use tauri_plugin_autostart::ManagerExt;
        let _ = app.autolaunch().enable();
    }
    Ok(())
}

fn load(app: &AppHandle) -> Option<Settings> {
    let path = settings_path(app);
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn get(app: &AppHandle) -> Settings {
    if let Some(state) = app.try_state::<SettingsState>() {
        let inner = state.inner().0.lock().unwrap();
        inner.clone()
    } else {
        load(app).unwrap_or_default()
    }
}

pub fn save(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = settings_path(app);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let raw = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    fs::write(&path, raw).map_err(|e| e.to_string())?;
    if let Some(state) = app.try_state::<SettingsState>() {
        *state.inner().0.lock().unwrap() = settings.clone();
    } else {
        app.manage(SettingsState(Mutex::new(settings.clone())));
    }
    Ok(())
}

/// Resolve the configured screenshot directory, falling back to the default if invalid.
pub fn resolved_dir(settings: &Settings) -> PathBuf {
    let dir = filename::expand_dir(&settings.screenshot_dir);
    match filename::ensure_dir_writable(&dir) {
        Ok(()) => dir,
        Err(e) => {
            log::warn!("configured dir invalid ({e}); using default");
            let def = filename::default_screenshot_dir();
            let _ = filename::ensure_dir_writable(&def);
            def
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let s = Settings::default();
        assert_eq!(s.hotkey, "Ctrl+Shift+4");
        assert!(s.screenshot_dir.contains("Pictures"));
        assert_eq!(s.max_history, 10);
        assert!(s.copy_to_clipboard);
        assert!(s.start_with_windows);
        assert_eq!(s.last_area_hotkey, "Ctrl+Alt+4");
        assert_eq!(s.video_hotkey, "Ctrl+Alt+V");
        assert!(s.last_area.is_none());
    }

    #[test]
    fn last_area_roundtrip() {
        let mut s = Settings::default();
        s.last_area = Some(LastArea {
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        });
        let raw = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.last_area, s.last_area);
        // Old settings files without the field default to None.
        let partial = serde_json::from_str::<Settings>("{}").unwrap();
        assert!(partial.last_area.is_none());
        assert_eq!(partial.last_area_hotkey, "Ctrl+Alt+4");
        assert_eq!(partial.video_hotkey, "Ctrl+Alt+V");
    }

    #[test]
    fn serde_roundtrip() {
        let s = Settings::default();
        let raw = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.hotkey, s.hotkey);
        // Missing fields fall back to defaults.
        let partial = serde_json::from_str::<Settings>("{}").unwrap();
        assert_eq!(partial.hotkey, "Ctrl+Shift+4");
    }
}
