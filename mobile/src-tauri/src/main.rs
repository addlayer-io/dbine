// Prevents an extra console window on Windows (desktop runs are for development).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    dbine_mobile_lib::run()
}
