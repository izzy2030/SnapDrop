//! Tiny file-based debug logger.
//!
//! Writes timestamped lines to `snapdrop-debug.log` inside the app config
//! directory. Used to diagnose environment-specific failures (stale thumbnail,
//! missed events) that only reproduce on the user's machine. Each app start
//! ROTATES the previous run's file to `snapdrop-debug.prev.log` instead of
//! truncating it: a wedged (not crashed) session produces no panic file, so
//! the debug log is the only evidence — it must survive the relaunch that
//! usually follows a hang.

use std::collections::hash_map::DefaultHasher;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tauri::Manager;

static LOG: Mutex<Option<PathBuf>> = Mutex::new(None);
static START: OnceLock<Instant> = OnceLock::new();
static SESSION_ID: OnceLock<String> = OnceLock::new();

/// Max bytes per session log file. The heartbeat/reconcile chatter plus an
/// error flood could otherwise grow one file forever and flush the
/// visibility/lifecycle history out of any readable window. On breach the
/// file rotates (dropping the older prev) with a marker line.
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

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

/// Point the logger at the app config dir (called once at startup). The
/// previous run's log is ROTATED to `snapdrop-debug.prev.log` instead of
/// being truncated: the failure modes this log exists to diagnose (a wedged
/// event loop, a dead thumbnail renderer, a black window) usually don't kill
/// the process — the app just hangs until the user quits it — and the next
/// launch is often how the user notices. Truncating on start therefore
/// destroyed the exact evidence needed. The previous session now survives.
pub fn init(app: &tauri::AppHandle) {
    let dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."));
    init_path(dir.join("snapdrop-debug.log"));
}

/// Point the logger at an explicit file and rotate whatever was there.
///
/// The app calls `init` (which delegates here). Headless harnesses have no
/// `AppHandle`, so they call this directly — otherwise `log` stays a no-op and
/// the recorder's health / audio-pump lines are never written anywhere.
pub fn init_path(path: PathBuf) {
    if path.exists() {
        let prev = path.with_extension("prev.log");
        let _ = std::fs::remove_file(&prev);
        let _ = std::fs::rename(&path, &prev);
    }
    let _ = OpenOptions::new().create(true).write(true).truncate(true).open(&path);
    // Drop the guard BEFORE logging: `log` re-locks this mutex, and std Mutex
    // is not reentrant — locking it twice on the same thread would deadlock
    // the main thread during setup (app appears frozen, hotkey never fires).
    let prev = path.with_extension("prev.log");
    let prev_status = prev_ended_cleanly(&prev);
    {
        let mut guard = LOG.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(path);
    }
    let id = SESSION_ID.get_or_init(new_session_id);
    log(&format!("===== session start id={id} ({}) =====", wall_clock()));
    match prev_status {
        None => log("previous session: none (first run, or no prev log)"),
        Some(true) => log("previous session: ended cleanly"),
        Some(false) => {
            log("previous session: UNCLEAN (no exit marker — may have wedged; see prev log)")
        }
    }
}

/// Unique id for this process run, so restart-vs-resume is greppable.
fn new_session_id() -> String {
    let mut h = DefaultHasher::new();
    std::process::id().hash(&mut h);
    SystemTime::now().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Whether the previous session's log ends with the clean-exit marker.
/// Reads only the last 4KB: the file is size-capped but a wedged session's
/// prev log is exactly the evidence this must not disturb. None = no prev log.
fn prev_ended_cleanly(prev: &PathBuf) -> Option<bool> {
    let data = std::fs::read(prev).ok()?;
    let tail = if data.len() > 4096 { &data[data.len() - 4096..] } else { &data[..] };
    Some(String::from_utf8_lossy(tail).contains("session end (clean exit)"))
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

/// Read the previous session's rotated log, if any.
pub fn read_previous(app: &tauri::AppHandle) -> Result<String, String> {
    let path = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("snapdrop-debug.prev.log");
    if !path.exists() {
        return Err("No previous session log yet (current session is the first, or the previous file was missing).".to_string());
    }
    std::fs::read_to_string(&path).map_err(|e| format!("Could not read {}: {e}", path.display()))
}

pub fn open(app: &tauri::AppHandle) -> Result<(), String> {
    let path = log_path(app);
    if !path.exists() {
        let _ = OpenOptions::new().create(true).append(true).open(&path);
    }
    crate::commands::open_with_default_app(&path.to_string_lossy())
}

/// Open the previous session's rotated log, if any.
pub fn open_previous(app: &tauri::AppHandle) -> Result<(), String> {
    let path = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("snapdrop-debug.prev.log");
    if !path.exists() {
        return Err("No previous session log yet.".to_string());
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
        // Size cap: rotate mid-session instead of growing forever.
        let rotated = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_LOG_BYTES;
        if rotated {
            let prev = path.with_extension("prev.log");
            let _ = std::fs::remove_file(&prev);
            let _ = std::fs::rename(path, &prev);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            if rotated {
                let _ = f.write_all(b"--- log rotated (size cap 2MB) ---\n");
            }
            let _ = f.write_all(line.as_bytes());
        }
    }
}
