use std::env;
use dotenv::dotenv;
use serde_json::{json, Value};

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}
#[tauri::command]
fn my_get_api_keys() -> Value {
    // Carica le variabili d'ambiente dal file .env
    dotenv().ok();

    // Legge le chiavi (se non le trova restituisce una stringa vuota)
    let gemini_api = env::var("GEMINI_API").unwrap_or_else(|_| "".to_string());
    let eleven_labs_key = env::var("ELEVEN_LABS_KEY").unwrap_or_else(|_| "".to_string());

    // Crea e restituisce un oggetto JSON
    json!({
        "GEMINI_API": gemini_api,
        "ELEVEN_LABS_KEY": eleven_labs_key
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![greet])
        .invoke_handler(tauri::generate_handler![my_get_api_keys])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
