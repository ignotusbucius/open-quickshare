/// Open a file, directory, or URL with the system handler. The shell
/// plugin's `open()` silently rejects without a configured scope, so this
/// goes straight to `xdg-open` (this app ships for Linux only).
#[tauri::command]
pub fn open_path(path: String) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("xdg-open failed: {e}"))
}
