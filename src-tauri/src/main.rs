// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    snapdrop_lib::init_dpi();
    snapdrop_lib::run()
}
