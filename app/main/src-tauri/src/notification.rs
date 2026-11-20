#[cfg(target_os = "linux")]
use crate::dto::{ChannelAction, ChannelDirection, FrontChannelMessage};
#[cfg(target_os = "linux")]
use notify_rust::Notification;
#[cfg(target_os = "linux")]
use rqs_lib::Visibility;
use tauri::AppHandle;
#[cfg(target_os = "linux")]
use tauri::Manager;
#[cfg(not(target_os = "linux"))]
use tauri_plugin_notification::NotificationExt;

#[cfg(target_os = "linux")]
use crate::cmds;

/// The app's name as notification servers should show it. Without this,
/// notify-rust sends the executable's name ("rquickshare").
pub const APP_NAME: &str = "Open QuickShare";

/// A notification carrying our name and icon.
#[cfg(target_os = "linux")]
fn notification() -> Notification {
    let mut n = Notification::new();
    n.appname(APP_NAME);
    if let Some(icon) = crate::integration::app_icon() {
        n.icon(&icon.to_string_lossy());
    }
    n
}

/// Plain notification (title + body), for the frontend.
#[tauri::command]
pub fn notify(app: AppHandle, title: String, body: String) {
    #[cfg(target_os = "linux")]
    {
        let _ = &app;
        if let Err(e) = notification().summary(&title).body(&body).show() {
            error!("Couldn't show notification: {e}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = app
        .notification()
        .builder()
        .title(&title)
        .body(&body)
        .show();
}

pub fn send_request_notification(name: String, id: String, app_handle: &AppHandle) {
    // Is not used in macos, get rid of warning
    let _ = id;

    let body = format!("{name} wants to share with you");

    #[cfg(not(target_os = "linux"))]
    let _ = app_handle
        .notification()
        .builder()
        .title(APP_NAME)
        .body(&body)
        .show();

    #[cfg(target_os = "linux")]
    match notification()
        .summary(APP_NAME)
        .body(&body)
        .action("accept", "Accept")
        .action("reject", "Reject")
        .show()
    {
        Ok(n) => {
            let capp_handle = app_handle.clone();
            // TODO - Meh, untracked, unwaited tasks...
            #[cfg(target_os = "linux")]
            tokio::task::spawn(async move {
                n.wait_for_action(|action| match action {
                    "accept" => {
                        let _ = cmds::send_to_rs(
                            FrontChannelMessage {
                                id,
                                direction: ChannelDirection::FrontToLib,
                                action: Some(ChannelAction::AcceptTransfer),
                                ..Default::default()
                            },
                            capp_handle.state(),
                        );
                    }
                    "reject" => {
                        let _ = cmds::send_to_rs(
                            FrontChannelMessage {
                                id,
                                direction: ChannelDirection::FrontToLib,
                                action: Some(ChannelAction::RejectTransfer),
                                ..Default::default()
                            },
                            capp_handle.state(),
                        );
                    }
                    _ => (),
                });
            });
        }
        Err(e) => {
            error!("Couldn't show notification: {}", e);
        }
    }
}

pub fn send_temporarily_notification(app_handle: &AppHandle) {
    let body = "Someone nearby is sharing — you're hidden".to_string();

    #[cfg(not(target_os = "linux"))]
    let _ = app_handle
        .notification()
        .builder()
        .title(APP_NAME)
        .body(&body)
        .show();

    #[cfg(target_os = "linux")]
    match notification()
        .summary(APP_NAME)
        .body(&body)
        .action("visible", "Be visible (1m)")
        .action("ignore", "Ignore")
        .id(1919)
        .show()
    {
        Ok(n) => {
            let capp_handle = app_handle.clone();
            // TODO - Meh, untracked, unwaited tasks...
            #[cfg(target_os = "linux")]
            tokio::task::spawn(async move {
                n.wait_for_action(|action| match action {
                    "visible" => {
                        cmds::change_visibility(Visibility::Temporarily, capp_handle.state());
                    }
                    "ignore" => {}
                    _ => (),
                });
            });
        }
        Err(e) => {
            error!("Couldn't show notification: {}", e);
        }
    }
}
