//! Tiny file-based debug logger.
//!
//! Writes timestamped lines to `snapdrop-debug.log` inside the app config
//! directory. Used to diagnose environment-specific failures (stale thumbnail,
//! missed events) that only reproduce on the user's machine; the log file is
//! truncated to a bounded size on each app start.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::Manager;

static LOG: Mutex<Option<PathBuf>> = Mutex::new(None);
static START: OnceLock<Instant> = OnceLock::new();

fn elapsed_secs() -> f64 {
    let start = *START.get_or_init(Instant::now);
    start.elapsed().as_secs_f64()
}

/// Wall-clock time for the first line of the log.
pub fn wall_clock() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() % 86400;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Point the logger at the app config dir (called once at startup). Truncates
/// the previous run's log so each session starts fresh.
pub fn init(app: &tauri::AppHandle) {
    let path = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("snapdrop-debug.log");
    let _ = OpenOptions::new().create(true).write(true).truncate(true).open(&path);
    // Drop the guard BEFORE logging: `log` re-locks this mutex, and std Mutex
    // is not reentrant — locking it twice on the same thread would deadlock
    // the main thread during setup (app appears frozen, hotkey never fires).
    {
        let mut guard = LOG.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(path);
    }
    log(&format!("===== session start ({}) =====", wall_clock()));
}

fn log_path(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("snapdrop-debug.log")
}

pub fn read(app: &tauri::AppHandle) -> Result<String, String> {
    let path = log_path(app);
    std::fs::read_to_string(&path).map_err(|e| format!("Could not read {}: {e}", path.display()))
}

pub fn open(app: &tauri::AppHandle) -> Result<(), String> {
    let path = log_path(app);
    if !path.exists() {
        let _ = OpenOptions::new().create(true).append(true).open(&path);
    }
    crate::commands::open_with_default_app(&path.to_string_lossy())
}

/// Append a timestamped line to the debug log (no-op before `init`).
pub fn log(msg: &str) {
    let secs = elapsed_secs();
    let line = format!("[{secs:8.3}] {msg}\n");
    // Recover from a poisoned mutex (a panic while the lock was held) so
    // logging keeps working — it's a diagnostics tool, it must not go quiet
    // exactly when a panic is being diagnosed.
    let guard = LOG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(path) = guard.as_ref() {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}
