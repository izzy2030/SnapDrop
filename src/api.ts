import { invoke } from "@tauri-apps/api/core";

export interface Settings {
  hotkey: string;
  screenshot_dir: string;
  format: string;
  start_with_windows: boolean;
  show_thumbnail: boolean;
  thumbnail_duration_secs: number;
  thumbnail_size: string;
  thumbnail_position: string;
  copy_to_clipboard: boolean;
  keep_after_drag: boolean;
  hide_after_drop: boolean;
  show_editor_after_capture: boolean;
  max_history: number;
  confirm_delete: boolean;
  close_to_tray: boolean;
}

export interface HistoryEntry {
  path: string;
  captured_at: string;
  size_bytes?: number;
  width?: number;
  height?: number;
}

export interface CapturedPayload {
  capture_id: number;
  path: string | null;
  preview: string; // data URL
  width: number;
  height: number;
  unsaved: boolean;
}

export interface DragOutcome {
  dropped: boolean;
  moved: boolean;
}

export interface ToastPayload {
  kind: "info" | "error";
  message: string;
}

export const api = {
  getSettings: () => invoke<Settings>("get_settings"),
  updateSettings: (settings: Settings) =>
    invoke<void>("update_settings", { settings }),
  getHistory: () => invoke<HistoryEntry[]>("get_history"),
  deleteCapture: (path: string) => invoke<void>("delete_capture", { path }),
  clearHistory: () => invoke<void>("clear_history"),
  openCapture: (path: string) => invoke<void>("open_capture", { path }),
  revealCapture: (path: string) => invoke<void>("reveal_capture", { path }),
  openFolder: () => invoke<void>("open_folder"),
  startDrag: (path: string) => invoke<DragOutcome>("start_drag", { path }),
  copyCapture: (path: string) => invoke<void>("copy_capture", { path }),
  captureNow: () => invoke<void>("capture_now"),
  hideThumbnail: () => invoke<void>("hide_thumbnail"),
  pauseHotkey: (paused: boolean) => invoke<void>("pause_hotkey", { paused }),
  getAppVersion: () => invoke<string>("get_app_version"),
  showSettings: () => invoke<void>("show_settings"),
  pickFolder: (current: string) =>
    invoke<string | null>("pick_folder", { current }),
  getCapturePreview: (path: string) =>
    invoke<string>("get_capture_preview", { path }),
  getLatestCapture: () => invoke<CapturedPayload | null>("get_latest_capture"),
  debugLog: (msg: string) => invoke<void>("debug_log", { msg }),
  reportRendererInput: () => invoke<void>("report_renderer_input"),
  reportRendererPointerDown: () => invoke<void>("report_renderer_pointerdown"),
  getDebugLog: () => invoke<string>("get_debug_log"),
  openDebugLog: () => invoke<void>("open_debug_log"),
};
