use crate::AppState;

/// The name shown to other devices. This is the custom name the user picked
/// (applied at startup), or the OS hostname when none was set.
#[tauri::command]
pub fn get_device_name(state: tauri::State<'_, AppState>) -> String {
    state.rqs.lock().unwrap().get_device_name()
}

/// Update the advertised device name. Takes effect immediately in the UI; the
/// name other devices see refreshes the next time the service starts (the store
/// value is passed to `RQS::new` at launch). An empty name is ignored -- the
/// frontend sends the OS hostname to reset to the default.
#[tauri::command]
pub fn set_device_name(name: String, state: tauri::State<'_, AppState>) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }

    info!("set_device_name: {name:?}");
    state.rqs.lock().unwrap().set_device_name(name.to_string());
}
