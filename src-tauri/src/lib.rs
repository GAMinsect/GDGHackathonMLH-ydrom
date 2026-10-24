use std::env;
use std::fs;
use dotenv::dotenv;
use serde_json::{json, Value};
use base64::{Engine as _, engine::general_purpose};
use rfd::FileDialog;

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command]
fn my_get_api_keys() -> Value {
    dotenv().ok();

    let gemini_api = env::var("GEMINI_API").unwrap_or_else(|_| "".to_string());
    let eleven_labs_key = env::var("ELEVEN_LABS_KEY").unwrap_or_else(|_| "".to_string());

    json!({
        "GEMINI_API": gemini_api,
        "ELEVEN_LABS_KEY": eleven_labs_key
    })
}

#[tauri::command]
fn save_censored_image(base64_image: String) -> Result<Option<String>, String> {
    // Rimuove l'intestazione "data:image/png;base64," se presente
    let base64_clean = base64_image
        .split(',')
        .nth(1)
        .unwrap_or(&base64_image);

    // Decodifica la stringa Base64 in byte
    let image_bytes = general_purpose::STANDARD
        .decode(base64_clean)
        .map_err(|e| format!("Errore di decodifica immagine: {}", e))?;

    // Apre il popup nativo del sistema operativo per scegliere dove salvare il file
    let file_path = FileDialog::new()
        .set_title("Salva documento censurato")
        .add_filter("Immagine PNG", &["png"])
        .set_file_name("documento_censurato.png")
        .save_file();

    if let Some(path) = file_path {
        fs::write(&path, image_bytes)
            .map_err(|e| format!("Errore durante il salvataggio del file: {}", e))?;
        Ok(Some(path.display().to_string()))
    } else {
        // L'utente ha annullato la finestra di dialogo
        Ok(None)
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            my_get_api_keys,
            save_censored_image
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}