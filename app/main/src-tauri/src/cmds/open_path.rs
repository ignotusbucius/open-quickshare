/// Open a file, directory, or URL with the system handler.
///
/// Two traps addressed here: the shell plugin's `open()` silently rejects
/// without a configured scope, and an AppImage poisons the environment
/// (LD_LIBRARY_PATH, GTK/GDK paths, XDG_DATA_DIRS) for any child it spawns —
/// so the launcher runs with those scrubbed. Tries `xdg-open`, then
/// `gio open`. Logs every step so a silent failure is diagnosable.
#[tauri::command]
pub fn open_path(path: String) -> Result<(), String> {
    info!("open_path: {path}");

    const POISON: &[&str] = &[
        "LD_LIBRARY_PATH",
        "LD_PRELOAD",
        "GDK_PIXBUF_MODULE_FILE",
        "GIO_MODULE_DIR",
        "GTK_PATH",
        "GTK_EXE_PREFIX",
        "GTK_DATA_PREFIX",
        "GTK_IM_MODULE_FILE",
        "GTK_THEME",
        "GST_PLUGIN_SYSTEM_PATH",
        "GST_PLUGIN_SYSTEM_PATH_1_0",
        "GSETTINGS_SCHEMA_DIR",
        "PYTHONHOME",
        "PYTHONPATH",
    ];

    let mut last_err = String::new();
    for opener in ["xdg-open", "gio"] {
        let mut cmd = std::process::Command::new(opener);
        if opener == "gio" {
            cmd.arg("open");
        }
        cmd.arg(&path);
        for var in POISON {
            cmd.env_remove(var);
        }
        // Strip AppImage-mount entries out of XDG_DATA_DIRS: xdg-open resolves
        // handlers through it, and the mount's share dirs shadow the system's.
        if let (Ok(appdir), Ok(xdg)) = (std::env::var("APPDIR"), std::env::var("XDG_DATA_DIRS")) {
            let cleaned = xdg
                .split(':')
                .filter(|p| !p.is_empty() && !p.starts_with(appdir.as_str()))
                .collect::<Vec<_>>()
                .join(":");
            cmd.env(
                "XDG_DATA_DIRS",
                if cleaned.is_empty() {
                    "/usr/local/share:/usr/share".to_string()
                } else {
                    cleaned
                },
            );
        }
        match cmd.spawn() {
            Ok(mut child) => {
                let opener = opener.to_string();
                std::thread::spawn(move || {
                    let status = child.wait();
                    info!("open_path: {opener} exited: {status:?}");
                });
                return Ok(());
            }
            Err(e) => {
                warn!("open_path: {opener} spawn failed: {e}");
                last_err = format!("{opener}: {e}");
            }
        }
    }
    error!("open_path: all openers failed: {last_err}");
    Err(format!("couldn't open: {last_err}"))
}
