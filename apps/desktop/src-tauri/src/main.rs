#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    println!("SlipStream {}", slipstream_desktop_lib::desktop_version());
}
