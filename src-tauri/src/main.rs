#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[tokio::main]
async fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running Quiet Rabbit");
}
