//! Global hotkey registration (tauri-plugin-global-shortcut) and capture trigger.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{
    Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
};

use crate::{capture_flow, notifier, settings};

pub static CAPTURING: AtomicBool = AtomicBool::new(false);

/// Whether the "Esc hides the thumbnail" hotkey is currently registered.
static ESC_REGISTERED: AtomicBool = AtomicBool::new(false);

pub struct CurrentShortcut(pub Mutex<Option<Shortcut>>);

pub fn current(app: &AppHandle) -> Option<Shortcut> {
    app.try_state::<CurrentShortcut>()
        .and_then(|s| s.inner().0.lock().unwrap().clone())
}

pub fn init(app: &AppHandle) {
    let settings = settings::get(app);
    let shortcut = match parse_shortcut(&settings.hotkey) {
        Ok(s) => s,
        Err(e) => {
            log::error!("invalid hotkey in settings: {e}; using default");
            Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Digit4)
        }
    };
    app.manage(CurrentShortcut(Mutex::new(Some(shortcut.clone()))));

    match app.global_shortcut().on_shortcut(shortcut.clone(), |app, _sc, event| {
        if event.state() == ShortcutState::Pressed {
            trigger_capture(app);
        }
    }) {
        Ok(()) => log::info!("hotkey registered: {}", shortcut_to_string(&shortcut)),
        Err(e) => {
            log::warn!("hotkey registration failed (conflict?): {e}");
            notifier::toast(
                app,
                "error",
                &format!(
                    "Hotkey {} is already in use by another app. Choose another in Settings.",
                    shortcut_to_string(&shortcut)
                ),
            );
        }
    }

    // Esc dismisses the floating thumbnail (registered once; acts only while
    // the thumbnail is visible).
    register_esc_hotkey(app);
}

/// Request a capture. Runs the full flow on the main thread with a reentrancy guard.
pub fn trigger_capture(app: &AppHandle) {
    if CAPTURING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        capture_flow::run(&app2);
        CAPTURING.store(false, Ordering::SeqCst);
    });
}

/// Re-register the hotkey from settings (used by update_settings). Returns Err on conflict.
pub fn apply_settings(app: &AppHandle, hotkey_str: &str) -> Result<(), String> {
    let shortcut = parse_shortcut(hotkey_str)?;
    let old = current(app);

    // Unregister the old one first so re-registering the same key succeeds.
    if let Some(o) = &old {
        let _ = app.global_shortcut().unregister(o.clone());
    }

    match app.global_shortcut().register(shortcut.clone()) {
        Ok(()) => {
            if let Some(state) = app.try_state::<CurrentShortcut>() {
                *state.inner().0.lock().unwrap() = Some(shortcut);
            }
            Ok(())
        }
        Err(e) => {
            // Roll back to the old registration.
            if let Some(o) = &old {
                let _ = app.global_shortcut().register(o.clone());
            }
            Err(format!("Could not register {hotkey_str}: {e}"))
        }
    }
}

pub fn pause(app: &AppHandle, paused: bool) {
    let sc = current(app);
    if paused {
        if let Some(s) = &sc {
            let _ = app.global_shortcut().unregister(s.clone());
        }
    } else if let Some(s) = &sc {
        let _ = app.global_shortcut().register(s.clone());
    }
}

/// Register Esc as a hotkey that hides the floating thumbnail. Registered once
/// at startup; the handler only acts while the thumbnail is visible, so Esc is
/// never swallowed at other times.
///
/// Note: global hotkeys consume the key — the webview never sees it. So when
/// the annotation editor is open, Esc is routed to the editor instead of the
/// thumbnail. The handler runs on the main thread (WM_HOTKEY → main message
/// pump), so it must NOT call `global_shortcut().unregister(...)` — that
/// dispatches back to the main thread and deadlocks.
pub fn register_esc_hotkey(app: &AppHandle) {
    if ESC_REGISTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    let esc = Shortcut::new(None, Code::Escape);
    match app.global_shortcut().on_shortcut(esc, |app, _sc, event| {
        if event.state() != ShortcutState::Pressed {
            return;
        }
        if crate::editor::is_visible(app) {
            let _ = app.emit("editor-cancelled", ());
        } else if crate::thumbnail::is_visible(app) {
            let _ = crate::thumbnail::hide_all(app);
        }
    }) {
        Ok(()) => log::info!("esc hotkey registered (dismiss)"),
        Err(e) => {
            log::warn!("esc hotkey registration failed: {e}");
            ESC_REGISTERED.store(false, Ordering::SeqCst);
        }
    }
}


pub    fn parse_shortcut(s: &str) -> Result<Shortcut, String> {
    let mut mods = Modifiers::empty();
    let mut code: Option<Code> = None;
    for part in s.split('+') {
        let p = part.trim().to_ascii_lowercase();
        match p.as_str() {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "shift" => mods |= Modifiers::SHIFT,
            "alt" => mods |= Modifiers::ALT,
            "super" | "win" | "cmd" => mods |= Modifiers::SUPER,
            "" => {}
            other => {
                let c = code_from_name(other).ok_or_else(|| format!("unknown key: {other}"))?;
                if code.is_some() {
                    return Err(format!("multiple keys in shortcut: {s}"));
                }
                code = Some(c);
            }
        }
    }
    let code = code.ok_or_else(|| format!("missing key in shortcut: {s}"))?;
    Ok(Shortcut::new(Some(mods), code))
}

pub fn shortcut_to_string(sc: &Shortcut) -> String {
    let mut parts: Vec<String> = Vec::new();
    let m = sc.mods;
    if m.contains(Modifiers::CONTROL) {
        parts.push("Ctrl".into());
    }
    if m.contains(Modifiers::SHIFT) {
        parts.push("Shift".into());
    }
    if m.contains(Modifiers::ALT) {
        parts.push("Alt".into());
    }
    if m.contains(Modifiers::SUPER) {
        parts.push("Win".into());
    }
    parts.push(name_from_code(sc.key));
    parts.join("+")
}

fn code_from_name(name: &str) -> Option<Code> {
    use Code::*;
    Some(match name {
        "a" => KeyA,
        "b" => KeyB,
        "c" => KeyC,
        "d" => KeyD,
        "e" => KeyE,
        "f" => KeyF,
        "g" => KeyG,
        "h" => KeyH,
        "i" => KeyI,
        "j" => KeyJ,
        "k" => KeyK,
        "l" => KeyL,
        "m" => KeyM,
        "n" => KeyN,
        "o" => KeyO,
        "p" => KeyP,
        "q" => KeyQ,
        "r" => KeyR,
        "s" => KeyS,
        "t" => KeyT,
        "u" => KeyU,
        "v" => KeyV,
        "w" => KeyW,
        "x" => KeyX,
        "y" => KeyY,
        "z" => KeyZ,
        "0" => Digit0,
        "1" => Digit1,
        "2" => Digit2,
        "3" => Digit3,
        "4" => Digit4,
        "5" => Digit5,
        "6" => Digit6,
        "7" => Digit7,
        "8" => Digit8,
        "9" => Digit9,
        "space" => Space,
        "enter" => Enter,
        "tab" => Tab,
        "escape" | "esc" => Escape,
        "backspace" => Backspace,
        "delete" => Delete,
        "home" => Home,
        "end" => End,
        "pageup" => PageUp,
        "pagedown" => PageDown,
        "insert" => Insert,
        "up" | "arrowup" => ArrowUp,
        "down" | "arrowdown" => ArrowDown,
        "left" | "arrowleft" => ArrowLeft,
        "right" | "arrowright" => ArrowRight,
        "`" => Backquote,
        "-" => Minus,
        "=" => Equal,
        "[" => BracketLeft,
        "]" => BracketRight,
        "\\" => Backslash,
        ";" => Semicolon,
        "'" => Quote,
        "," => Comma,
        "." => Period,
        "/" => Slash,
        "f1" => F1,
        "f2" => F2,
        "f3" => F3,
        "f4" => F4,
        "f5" => F5,
        "f6" => F6,
        "f7" => F7,
        "f8" => F8,
        "f9" => F9,
        "f10" => F10,
        "f11" => F11,
        "f12" => F12,
        "f13" => F13,
        "f14" => F14,
        "f15" => F15,
        "f16" => F16,
        "f17" => F17,
        "f18" => F18,
        "f19" => F19,
        "f20" => F20,
        "f21" => F21,
        "f22" => F22,
        "f23" => F23,
        "f24" => F24,
        _ => return None,
    })
}

fn name_from_code(code: Code) -> String {
    use Code::*;
    match code {
        Space => "Space".into(),
        Enter => "Enter".into(),
        Tab => "Tab".into(),
        Escape => "Esc".into(),
        Backspace => "Backspace".into(),
        Delete => "Delete".into(),
        Home => "Home".into(),
        End => "End".into(),
        PageUp => "PageUp".into(),
        PageDown => "PageDown".into(),
        Insert => "Insert".into(),
        ArrowUp => "Up".into(),
        ArrowDown => "Down".into(),
        ArrowLeft => "Left".into(),
        ArrowRight => "Right".into(),
        Backquote => "`".into(),
        Minus => "-".into(),
        Equal => "=".into(),
        BracketLeft => "[".into(),
        BracketRight => "]".into(),
        Backslash => "\\".into(),
        Semicolon => ";".into(),
        Quote => "'".into(),
        Comma => ",".into(),
        Period => ".".into(),
        Slash => "/".into(),
        other => {
            let dbg = format!("{other:?}");
            if let Some(rest) = dbg.strip_prefix("Key") {
                if rest.len() == 1 {
                    return rest.to_ascii_lowercase();
                }
            }
            if let Some(rest) = dbg.strip_prefix("Digit") {
                return rest.to_string();
            }
            dbg
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_default_hotkey() {
        let sc = parse_shortcut("Ctrl+Shift+4").unwrap();
        assert!(sc.mods.contains(Modifiers::CONTROL));
        assert!(sc.mods.contains(Modifiers::SHIFT));
        assert_eq!(sc.key, Code::Digit4);
        assert_eq!(shortcut_to_string(&sc), "Ctrl+Shift+4");
    }

    #[test]
    fn parse_alt_forms() {
        let sc = parse_shortcut("Alt+F2").unwrap();
        assert!(sc.mods.contains(Modifiers::ALT));
        assert_eq!(sc.key, Code::F2);
        let sc2 = parse_shortcut("win+shift+Space").unwrap();
        assert!(sc2.mods.contains(Modifiers::SUPER));
        assert_eq!(sc2.key, Code::Space);
    }

    #[test]
    fn reject_bad_shortcut() {
        assert!(parse_shortcut("Ctrl+NotAKey").is_err());
        assert!(parse_shortcut("Ctrl+Shift").is_err());
    }
}
