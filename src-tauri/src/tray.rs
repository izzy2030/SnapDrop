//! System tray icon and menu.

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::AppHandle;

use crate::{commands, history, hotkey, thumbnail, video_recording};

pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app)?;
    let icon = match app.default_window_icon().cloned() {
        Some(icon) => icon,
        None => tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))
            .expect("embedded 32x32.png icon must be valid PNG"),
    };
    TrayIconBuilder::with_id("main")
        .tooltip("SnapDrop")
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(handle_menu_event)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                // Left click opens Settings.
                let app = tray.app_handle();
                let _ = commands::show_settings_inner(app);
            }
        })
        .build(app)?;
    Ok(())
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let paused = crate::PAUSED.load(std::sync::atomic::Ordering::SeqCst);

    let entries = history::entries(app);
    let mut recent_items: Vec<tauri::menu::MenuItem<tauri::Wry>> = Vec::new();
    if entries.is_empty() {
        let item = MenuItem::with_id(app, "recent:none", "No captures yet", false, None::<&str>)?;
        recent_items.push(item);
    } else {
        for (i, e) in entries.iter().enumerate() {
            let label = e
                .path
                .rsplit('\\')
                .next()
                .unwrap_or(&e.path)
                .to_string();
            let item = MenuItem::with_id(app, format!("recent:{i}"), label, true, None::<&str>)?;
            recent_items.push(item);
        }
    }
    let recent_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        recent_items.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect();
    let recent = Submenu::with_items(app, "Recent Captures", true, &recent_refs)?;

    let pause_label = if paused { "Resume Hotkey" } else { "Pause Hotkey" };
    let recording = video_recording::is_recording_active();
    let settings_shortcut = hotkey::current(app)
        .map(|s| hotkey::shortcut_to_string(&s))
        .unwrap_or_else(|| "Ctrl+Shift+4".to_string());

    let capture_item =
        MenuItem::with_id(app, "capture", "Capture Region", true, Some(&settings_shortcut))?;
    let settings_item = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let open_folder_item =
        MenuItem::with_id(app, "open_folder", "Open Screenshot Folder", true, None::<&str>)?;
    let pause_item = MenuItem::with_id(app, "pause", pause_label, true, None::<&str>)?;
    let about_item = MenuItem::with_id(app, "about", "About SnapDrop", true, None::<&str>)?;
    let exit_item = MenuItem::with_id(app, "exit", "Exit", true, None::<&str>)?;
    let stop_item = MenuItem::with_id(app, "video_stop", "Stop Recording", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;

    let mut menu_items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![
        &capture_item,
        &settings_item,
        &recent,
        &open_folder_item,
    ];
    if recording {
        menu_items.push(&stop_item);
    }
    menu_items.push(&pause_item);
    menu_items.push(&sep);
    menu_items.push(&about_item);
    menu_items.push(&sep);
    menu_items.push(&exit_item);
    let menu = Menu::with_items(app, &menu_items)?;
    Ok(menu)
}

pub fn refresh(app: &AppHandle) {
    if let Ok(menu) = build_menu(app) {
        if let Some(tray) = app.tray_by_id("main") {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id().as_ref();
    match id {
        "capture" => hotkey::trigger_capture(app),
        "settings" => {
            let _ = commands::show_settings_inner(app);
        }
        "video_stop" => {
            let app = app.clone();
            std::thread::spawn(move || {
                if let Err(e) = video_recording::stop_recording(&app) {
                    log::warn!("stop recording failed: {e}");
                }
                crate::tray::refresh(&app);
            });
        }
        "open_folder" => {
            let _ = commands::open_folder_inner(app);
        }
        "pause" => {
            let paused = crate::PAUSED
                .fetch_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, |p| {
                    Some(!p)
                })
                .unwrap_or(false);
            hotkey::pause(app, paused);
            refresh(app);
        }
        "about" => {
            let _ = commands::show_settings_inner(app);
        }
        "exit" => {
            app.exit(0);
        }
        id if id.starts_with("recent:") => {
            if let Ok(idx) = id["recent:".len()..].parse::<usize>() {
                if let Some(e) = history::entries(app).get(idx) {
                    // Videos can't be shown as image thumbnails — open them in
                    // the default player instead.
                    if e.kind == "video"
                        || e.path.to_ascii_lowercase().ends_with(".mp4")
                    {
                        let _ = commands::open_with_default_app(&e.path);
                    } else {
                        let _ = thumbnail::show_capture_for(app, &e.path);
                    }
                }
            }
        }
        _ => {}
    }
}


