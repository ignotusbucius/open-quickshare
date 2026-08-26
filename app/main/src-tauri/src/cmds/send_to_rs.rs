use crate::AppState;
use crate::dto::{FrontChannelMessage, to_lib_message};

#[tauri::command]
pub fn send_to_rs(
    message: FrontChannelMessage,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    info!("send_to_rs: {:?}", &message);

    match state.message_sender.send(to_lib_message(message)) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Coudln't perform: {}", e)),
    }
}
