# Tauri 2 Windows Tray: Lethargy & Blank Screen Guide

A reference guide for diagnosing and fixing two notorious issues when minimizing Tauri 2 applications to the Windows system tray:
1. **Tray "Lethargy" / Click Unresponsiveness:** Clicking the tray icon requires multiple clicks before the window finally opens.
2. **Blank White Screen After Sleep/Idle:** When the window is restored after hours in the tray or after display sleep, it renders as a frozen, solid white box.

---

## 1. The Anatomy of the Defects

### Defect A: Tao Window State Desync (`diff == empty`)
* **How it happens:** If close-to-tray hides the OS window using direct Win32 `ShowWindow(hwnd, SW_HIDE)` (e.g. to bypass a busy main thread), Tao (Tauri’s window library) is never notified that the window was hidden.
* **The failure:** Tao caches window state (`WindowFlags::VISIBLE`). When you later click the tray icon and call `w.show()`, Tao compares `old_flags.VISIBLE` (`true`) with `new_flags.VISIBLE` (`true`).
  ```rust
  // tao/src/platform_impl/windows/window_state.rs
  let mut diff = self ^ new;
  if diff == WindowFlags::empty() {
      return; // <-- NO-OP! ShowWindow(SW_SHOW) is NEVER called!
  }
  ```
  Tao assumes the window is already visible and returns without calling Win32 `ShowWindow(SW_SHOW)`. The window remains invisible to the user.

### Defect B: Dropped Double-Clicks in `on_tray_icon_event`
* **How it happens:** Windows sends `WM_LBUTTONDBLCLK` when a user double-clicks a tray icon. Tauri translates this into `TrayIconEvent::DoubleClick`.
* **The failure:** If your tray handler only matches `TrayIconEvent::Click`, double-clicks (the natural Windows reflex when an app feels slow) are silently dropped.

### Defect C: Chromium Background Occlusion & Sleeping Tabs
* **How it happens:** By default, Microsoft Edge / Chromium aggressively throttles or suspends windows that are not visible (`CalculateNativeWinOcclusion`, background timer throttling, and process priority demotion).
* **The failure:** After being hidden in the tray for extended periods, JavaScript timers halt, the rendering pipeline freezes, and memory saver discards active buffers.

### Defect D: GPU Device Loss Across Display Sleep
* **How it happens:** When Windows monitors sleep or recover from power states, DirectX triggers device loss (`DXGI_ERROR_DEVICE_REMOVED` / `DXGI_ERROR_DEVICE_RESET`).
* **The failure:** Chromium's GPU process or DirectComposition swapchain disconnects. Because Tao’s `w.show()` does not re-invoke `ICoreWebView2Controller::SetIsVisible(true)` or notify Chromium of the state change, the window frame appears but the client area remains solid white.

---

## 2. The Complete Fix Pattern

Apply these 5 patterns to any Tauri app that minimizes to the tray on Windows:

### Step 1: Harden WebView2 Environment Variables (Rust)
At the very entry of your application (`run()` or `main()`), before `tauri::Builder` runs:

```rust
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(windows)]
    {
        // Prevent Chromium from suspending occluded windows, killing renderers,
        // or throttling background timers when hidden in the tray for hours.
        std::env::set_var(
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
            "--disable-backgrounding-occluded-windows --disable-background-timer-throttling --disable-renderer-backgrounding",
        );
    }

    tauri::Builder::default()
        // ...
}
```

---

### Step 2: Handle Both Click and Double-Click (Rust)
In your tray setup (`tray.rs`):

```rust
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

TrayIconBuilder::with_id("main")
    .tooltip("MyApp")
    .icon(icon)
    .menu(&menu)
    .show_menu_on_left_click(false)
    .on_tray_icon_event(|tray, event| {
        match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => {
                let app = tray.app_handle();
                let _ = show_main_window(app);
            }
            _ => {}
        }
    })
    .build(app)?;
```

---

### Step 3: Raw Win32 Show Helper (Rust)
In a native Win32 helper module (`native_win.rs`), expose direct OS window management functions:

```rust
use std::ffi::c_void;
type HWND = *mut c_void;

/// Show and activate the window directly via Win32, bypassing Tao diffing.
pub unsafe fn show_window_foreground(hwnd: *mut c_void) {
    if hwnd.is_null() {
        return;
    }
    extern "system" {
        fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> i32;
        fn SetForegroundWindow(hWnd: HWND) -> i32;
    }
    const SW_SHOWNORMAL: i32 = 1;
    unsafe {
        let _ = ShowWindow(hwnd as HWND, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd as HWND);
    }
}

/// Hide the window directly via Win32.
pub unsafe fn hide_window(hwnd: *mut c_void) {
    if hwnd.is_null() {
        return;
    }
    extern "system" {
        fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> i32;
    }
    const SW_HIDE: i32 = 0;
    unsafe {
        let _ = ShowWindow(hwnd as HWND, SW_HIDE);
    }
}
```

---

### Step 4: Synchronized Show/Hide with Staleness Auto-Revive (Rust)
Keep Tao and Win32 in lockstep, and auto-reload the page if it has been dormant across sleep:

```rust
static LAST_RENDERER_HEARTBEAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_renderer_heartbeat() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    LAST_RENDERER_HEARTBEAT.store(now, std::sync::atomic::Ordering::SeqCst);
}

pub fn is_renderer_stale() -> bool {
    let last = LAST_RENDERER_HEARTBEAT.load(std::sync::atomic::Ordering::SeqCst);
    if last == 0 {
        return true;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now.saturating_sub(last) > 10_000 // Stale if no heartbeat for >10s
}

pub fn show_main_window(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        // 1. OS-level force show + bring to foreground
        if let Ok(hwnd) = w.hwnd() {
            unsafe {
                crate::native_win::show_window_foreground(hwnd.0 as *mut std::ffi::c_void);
            }
        }
        // 2. Synchronize Tao & Wry visibility
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();

        // 3. Auto-revive if frozen or disconnected from swapchain
        if is_renderer_stale() {
            let _ = w.reload();
        }
    }
    Ok(())
}
```

And in your `on_window_event` (`CloseRequested`):

```rust
.on_window_event(|window, event| {
    if window.label() == "main" {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            if close_to_tray_enabled {
                api.prevent_close();
                // Synchronously update Tao's internal state
                let _ = window.hide();
                // Ensure the Win32 HWND is hidden instantly
                if let Ok(hwnd) = window.hwnd() {
                    unsafe {
                        crate::native_win::hide_window(hwnd.0 as *mut std::ffi::c_void);
                    }
                }
            }
        }
    }
})
```

---

### Step 5: Frontend Heartbeat & Keyboard Reload (React / Webview)
In your root application component (e.g. `App.tsx` / `SettingsApp.tsx`):

```tsx
import { useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";

export default function App() {
  // Liveness heartbeat: ping Rust on mount and every 4s while alive
  useEffect(() => {
    void invoke("report_renderer_heartbeat");
    const timer = setInterval(() => {
      void invoke("report_renderer_heartbeat");
    }, 4000);
    return () => clearInterval(timer);
  }, []);

  // Standard developer & user rescue shortcuts (F5 / Ctrl+R)
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "F5" || (e.ctrlKey && e.key.toLowerCase() === "r")) {
        e.preventDefault();
        window.location.reload();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);

  return <div>Your App Content</div>;
}
```

---

### Step 6: Dev-Instance Exception for `tauri-plugin-single-instance` (Rust)
If `tauri-plugin-single-instance` is registered unconditionally, running `npm run tauri dev` will detect the named mutex (`<identifier>-sim`) created by an already-running installed build in your Windows tray and immediately exit with code 0.

To allow the dev preview to run freely alongside your installed production app:

```rust
    let builder = tauri::Builder::default();

    // Only enforce single-instance in release builds.
    // In dev mode (`npm run tauri dev`), debug_assertions is true, so the plugin is skipped
    // and your dev preview launches freely side-by-side with the installed background app.
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        let _ = show_main_window(app);
    }));

    builder
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        // ...
```

---

## 3. Quick Verification Checklist

When building or updating a tray app:
- [ ] Does `on_tray_icon_event` listen to `DoubleClick` as well as `Click`?
- [ ] Does `CloseRequested` invoke `window.hide()` so Tao’s `WindowFlags::VISIBLE` is cleared?
- [ ] Does `show_window` call direct Win32 `SW_SHOWNORMAL` to bypass empty Tao flag diffs?
- [ ] Is `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` configured before builder init?
- [ ] Is there an active renderer heartbeat that triggers `w.reload()` if restoring after sleep?
- [ ] Do `single_instance` callbacks call the unified `show_main_window` helper?
- [ ] Is `tauri_plugin_single_instance` gated behind `#[cfg(not(debug_assertions))]` so dev runs side-by-side with the installed app?
