//! Clipboard access that works on Wayland.
//!
//! `tauri-plugin-clipboard-manager` is backed by `arboard`, which on Linux
//! speaks X11 only: on a Wayland session without XWayland every read and
//! write fails. GTK — the toolkit this window already runs on — owns a
//! native connection to whichever display server is in use, so on Linux the
//! clipboard goes through GTK first (on the main thread, where GTK lives) and
//! the plugin is only the fallback.

#[cfg(target_os = "linux")]
use std::time::Duration;

use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;

/// How long to wait for the clipboard owner to answer a read. A misbehaving
/// owner must not hang the paste forever.
#[cfg(target_os = "linux")]
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Copy `text` to the system clipboard.
#[tauri::command]
pub async fn copy_text(app: AppHandle, text: String) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    match gtk_set_text(&app, text.clone()).await {
        Ok(()) => return Ok(()),
        Err(e) => warn!("copy_text: GTK clipboard failed ({e}), falling back"),
    }

    app.clipboard().write_text(text).map_err(|e| {
        error!("copy_text: {e}");
        String::from("Couldn't access the clipboard")
    })
}

/// Read the clipboard as text. A file-manager copy that only offers a URI
/// list comes back as one `file://` URI per line.
pub async fn read_text(app: &AppHandle) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    match gtk_read_text(app).await {
        Ok(Some(text)) => return Ok(text),
        Ok(None) => trace!("read_text: GTK clipboard has no text"),
        Err(e) => warn!("read_text: GTK clipboard failed ({e}), falling back"),
    }

    app.clipboard()
        .read_text()
        .map_err(|_| String::from("Clipboard is empty or has no text"))
}

#[cfg(target_os = "linux")]
async fn gtk_set_text(app: &AppHandle, text: String) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
        // GTK keeps serving the selection for as long as the app runs.
        clipboard.set_text(&text);
        let _ = tx.send(());
    })
    .map_err(|e| e.to_string())?;
    rx.await
        .map_err(|_| String::from("main thread dropped the request"))
}

#[cfg(target_os = "linux")]
async fn gtk_read_text(app: &AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
    app.run_on_main_thread(move || {
        let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
        // Async requests rather than wait_for_*: those spin a nested main
        // loop on the UI thread.
        clipboard.request_text(move |clipboard, text| {
            if let Some(text) = text {
                let _ = tx.send(Some(text.to_owned()));
                return;
            }
            clipboard.request_uris(move |_, uris| {
                let _ = tx.send((!uris.is_empty()).then(|| {
                    uris.iter()
                        .map(|u| u.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                }));
            });
        });
    })
    .map_err(|e| e.to_string())?;

    match tokio::time::timeout(READ_TIMEOUT, rx).await {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(_)) => Err(String::from("main thread dropped the request")),
        Err(_) => Err(String::from("timed out waiting for the clipboard owner")),
    }
}
