//! "Send via Quick Share" from the file manager.
//!
//! * `open-quickshare --send <files…>` (paths or `file://` URIs) opens the
//!   window with those files ready to send. A second launch hands its argv to
//!   the running instance (single-instance plugin), so the menu entry works
//!   whether or not the app is already open.
//! * The app installs, for the current user, a Dolphin service menu and a
//!   Nautilus script that call the above, pointing at whatever is running
//!   right now (the AppImage file, or the installed binary). A setting removes
//!   them again.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager};

/// Files handed to us via `--send`, waiting for the UI to pick them up.
/// `Some(empty)` = a request whose selection held no regular files.
#[derive(Default)]
pub struct PendingSend(pub Mutex<Option<Vec<String>>>);

/// Marker line in every file we write, so we only ever replace/remove our own.
const MARKER: &str = "Written by Open QuickShare";
const DOLPHIN_FILE: &str = "open-quickshare-send.desktop";
const NAUTILUS_FILE: &str = "Send via Quick Share";

/// The paths after `--send`, made absolute against `cwd`, keeping regular
/// files only. `None` if there is no `--send` at all.
pub fn parse_send_args<I: IntoIterator<Item = String>>(args: I, cwd: &Path) -> Option<Vec<String>> {
    let mut args = args.into_iter().skip_while(|a| a != "--send");
    args.next()?; // the flag itself
    let files = args
        .filter_map(|a| {
            let p = if a.starts_with("file://") {
                url::Url::parse(&a).ok()?.to_file_path().ok()?
            } else {
                PathBuf::from(a)
            };
            let p = if p.is_absolute() { p } else { cwd.join(p) };
            if p.is_file() {
                Some(p.to_string_lossy().into_owned())
            } else {
                warn!("--send: skipping {p:?} (not a regular file)");
                None
            }
        })
        .collect();
    Some(files)
}

/// Queue files for sending and bring the window up; the UI collects them
/// with `take_pending_send` (on start-up, and on the event below).
pub fn queue_send(app: &AppHandle, files: Vec<String>) {
    info!("--send: {} file(s)", files.len());
    if let Some(state) = app.try_state::<PendingSend>() {
        state
            .0
            .lock()
            .unwrap()
            .get_or_insert_with(Vec::new)
            .extend(files);
    }
    let _ = app.emit("rs2js_send_files", ());
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[tauri::command]
pub fn take_pending_send(state: tauri::State<'_, PendingSend>) -> Option<Vec<String>> {
    state.0.lock().unwrap().take()
}

#[tauri::command]
pub fn set_file_manager_menu(app: AppHandle, enabled: bool) -> Result<(), String> {
    crate::store::set_file_manager_menu(&app, enabled);
    sync_file_manager_menu(enabled)
}

/// What the menu entries should launch, or None when there is no stable path
/// (an AppImage run from its extracted tree, whose files are temporary).
fn launcher() -> Option<PathBuf> {
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        return Some(PathBuf::from(appimage));
    }
    if std::env::var_os("APPDIR").is_some() {
        return None;
    }
    std::env::current_exe().ok()
}

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// Quote a path for a desktop-entry Exec= line.
fn desktop_exec_quote(p: &Path) -> String {
    let mut s = String::from("\"");
    for c in p.to_string_lossy().chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('"');
    s
}

/// Quote a path for a POSIX shell.
fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

/// Write `content` to `path` (mode 0755) unless an unrelated file is there.
fn write_ours(path: &Path, content: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == content {
            return Ok(());
        }
        if !existing.contains(MARKER) {
            warn!("not touching {path:?}: it wasn't written by us");
            return Ok(());
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content)?;
    // KF6 ignores user service menus that aren't executable; Nautilus scripts
    // must be executable to run at all.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

fn remove_ours(path: &Path) {
    if std::fs::read_to_string(path).is_ok_and(|s| s.contains(MARKER)) {
        let _ = std::fs::remove_file(path);
    }
}

/// Install (or remove) the file-manager entries for the current user.
pub fn sync_file_manager_menu(enabled: bool) -> Result<(), String> {
    let Some(data) = data_home() else {
        return Err("no home directory".into());
    };
    let dolphin = data.join("kio/servicemenus").join(DOLPHIN_FILE);
    let nautilus = data.join("nautilus/scripts").join(NAUTILUS_FILE);
    let icon = data.join("open-quickshare/send-icon.png");

    if !enabled {
        remove_ours(&dolphin);
        remove_ours(&nautilus);
        return Ok(());
    }
    let Some(exe) = launcher() else {
        debug!("file-manager menu: no stable launcher path (extracted AppImage); leaving entries as they are");
        return Ok(());
    };

    let mut errors = Vec::new();
    if let Some(dir) = icon.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&icon, include_bytes!("../icons/128x128.png"));

    if on_path("dolphin") || dolphin.exists() {
        let entry = format!(
            "[Desktop Entry]\n\
             # {MARKER}; it keeps this file pointed at the app. Turn it off in\n\
             # Settings → \"Send via Quick Share\" in file managers.\n\
             Type=Service\n\
             MimeType=all/allfiles;\n\
             X-KDE-ServiceTypes=KonqPopupMenu/Plugin\n\
             X-KDE-Priority=TopLevel\n\
             Actions=sendViaQuickShare\n\
             \n\
             [Desktop Action sendViaQuickShare]\n\
             Name=Send via Quick Share\n\
             Icon={}\n\
             Exec={} --send %F\n",
            icon.display(),
            desktop_exec_quote(&exe),
        );
        if let Err(e) = write_ours(&dolphin, &entry) {
            errors.push(format!("Dolphin: {e}"));
        }
    }
    if on_path("nautilus") || nautilus.exists() {
        // Nautilus runs scripts in the current folder with the selection as
        // (relative) arguments. Make them absolute here: an AppImage changes
        // into its own mount before our code runs, so relative names would
        // resolve against the wrong directory.
        let script = format!(
            "#!/bin/sh\n# {MARKER}; turn it off in Settings.\n\
             for f in \"$@\"; do set -- \"$@\" \"$(realpath -- \"$f\")\"; shift; done\n\
             exec {} --send \"$@\"\n",
            sh_quote(&exe),
        );
        if let Err(e) = write_ours(&nautilus, &script) {
            errors.push(format!("Nautilus: {e}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_send_args() {
        let dir = std::env::temp_dir().join(format!("oqs-send-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a b.txt");
        std::fs::write(&f, "x").unwrap();

        let none = parse_send_args(["app".to_string()], &dir);
        assert_eq!(none, None);

        let uri = url::Url::from_file_path(&f).unwrap().to_string();
        let got = parse_send_args(
            ["app", "--send", "a b.txt", &uri, "missing.txt", "."].map(String::from),
            &dir,
        )
        .unwrap();
        let want = f.to_string_lossy().into_owned();
        assert_eq!(got, vec![want.clone(), want]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quotes_exec_paths() {
        assert_eq!(
            desktop_exec_quote(Path::new("/a b/$x\"y")),
            r#""/a b/\$x\"y""#
        );
        assert_eq!(sh_quote(Path::new("/it's here")), r"'/it'\''s here'");
    }
}
