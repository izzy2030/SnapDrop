//! Emits transient notifications: an in-app `toast` event to the frontend
//! windows, plus a best-effort native Windows toast (works even when no
//! SnapDrop window is visible — e.g. right after a hotkey capture). The
//! native path needs the installed app's Start-Menu shortcut (AUMID
//! `com.snapdrop.desktop`) and silently does nothing in unpackaged dev runs.

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
    // Native toast as a secondary channel; failure is fine (event above still
    // reaches any open window).
    let _ = native_toast(message);
}

fn native_toast(message: &str) -> Result<(), String> {
    use windows::core::h;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    let escaped: String = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let xml = format!(
        "<toast duration=\"short\"><visual><binding template=\"ToastGeneric\"><text>{escaped}</text></binding></visual></toast>"
    );
    let doc = XmlDocument::new().map_err(|e| e.to_string())?;
    doc.LoadXml(&windows::core::HSTRING::from(xml.as_str()))
        .map_err(|e| e.to_string())?;
    let toast = ToastNotification::CreateToastNotification(&doc).map_err(|e| e.to_string())?;
    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(h!("com.snapdrop.desktop"))
            .map_err(|e| e.to_string())?;
    notifier.Show(&toast).map_err(|e| e.to_string())
}
