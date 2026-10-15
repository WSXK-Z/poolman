// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    desktop_lib::run()
}
// Tauri 二进制入口；应用生命周期与网关初始化由 desktop_lib 管理。