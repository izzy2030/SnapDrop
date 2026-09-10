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
  delete_files_on_remove: boolean;
  close_to_tray: boolean;
  ocr_hotkey: string;
  capture_delay_secs: number;
  last_area_hotkey: string;
  video_hotkey: string;
  video_fps: number;
  video_quality?: string;
  last_area: { x: number; y: number; width: number; height: number } | null;
}

export interface HistoryEntry {
  path: string;
  captured_at: string;
  size_bytes?: number;
  width?: number;
  height?: number;
  kind?: string;
  duration_secs?: number;
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
  startDrag: (paths: string[]) => invoke<DragOutcome>("start_drag", { paths }),
  copyCapture: (path: string) => invoke<void>("copy_capture", { path }),
  captureNow: () => invoke<void>("capture_now"),
  captureVideoNow: () => invoke<void>("capture_video_now"),
  videoRecordState: () => invoke<boolean>("video_record_state"),
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
  getPrevDebugLog: () => invoke<string>("get_prev_debug_log"),
  openDebugLog: () => invoke<void>("open_debug_log"),
  openPrevDebugLog: () => invoke<void>("open_prev_debug_log"),
  reportMainRendererReady: () => invoke<void>("report_main_renderer_ready"),
  ocrPendingEditorImage: () => invoke<string>("ocr_pending_editor_image"),
};
