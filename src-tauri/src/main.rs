// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic".to_string()
        };

        let mut bt = String::new();
        std::backtrace::Backtrace::force_capture()
            .to_string()
            .lines()
            .take(40)
            .for_each(|l| bt.push_str(&format!("\n    {l}")));

        let msg = format!("SnapDrop panic:\n\nError: {payload}\nLocation: {location}\nBacktrace:{bt}\n\nTimestamp: {:?}", std::time::SystemTime::now());
        let _ = std::fs::write(std::env::temp_dir().join("snapdrop_panic.txt"), &msg);

        #[cfg(windows)]
        unsafe {
            use windows::core::PCWSTR;
            use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
            let wide_msg: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
            let wide_title: Vec<u16> = "SnapDrop Crash Report".encode_utf16().chain(std::iter::once(0)).collect();
            let _ = MessageBoxW(
                None,
                PCWSTR(wide_msg.as_ptr()),
                PCWSTR(wide_title.as_ptr()),
                MB_OK | MB_ICONERROR,
            );
        }
    }));

    snapdrop_lib::init_dpi();
    snapdrop_lib::run()
}
