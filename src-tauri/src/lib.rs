//! Tauri app shell. Commands here stay thin: they call into `rowlark-core`
//! and return the result to the UI over IPC.

/// Smoke-test command: lets the UI confirm the Rust side is reachable and
/// shows which core version it was built with.
#[tauri::command]
fn core_version() -> String {
    rowlark_core::version().to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![core_version])
        // `expect` is acceptable here: if the app cannot start there is nothing to recover.
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
