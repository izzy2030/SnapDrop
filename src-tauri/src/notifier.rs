//! Emits transient toast notifications to the frontend windows.

use serde::Serialize;
use tauri::{AppHandle, Emitter};

#[derive(Serialize, Clone)]
pub struct ToastPayload {
    pub kind: String,
    pub message: String,
}

pub fn toast(app: &AppHandle, kind: &str, message: &str) {
    let _ = app.emit(
        "toast",
        ToastPayload {
            kind: kind.to_string(),
            message: message.to_string(),
        },
    );
}
