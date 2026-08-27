use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri_plugin_clipboard_manager::ClipboardExt;

/// What a paste resolved to: the file paths that were on the clipboard, or a
/// single generated .txt holding the clipboard text (Windows Quick Share style).
#[derive(Debug, Serialize)]
pub struct ResolvedPaste {
    pub paths: Vec<String>,
    pub is_text: bool,
}

const MAX_TEXT_BYTES: usize = 10 * 1024 * 1024;

/// Reads the clipboard and decides how to share it. A file-manager copy lands
/// in the text flavor as `file://` URIs (KDE, GNOME) or bare absolute paths;
/// only when EVERY line looks like one is the clipboard treated as files, so
/// a log excerpt containing one real path is still shared as text.
#[tauri::command]
pub async fn resolve_paste(app: tauri::AppHandle) -> Result<ResolvedPaste, String> {
    let text = app
        .clipboard()
        .read_text()
        .map_err(|_| String::from("Clipboard is empty or has no text"))?;

    if text.trim().is_empty() {
        return Err(String::from("Clipboard is empty"));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(String::from("Clipboard text is too large to share"));
    }

    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    // `file://` URIs are an unambiguous file-manager copy; the `url` crate
    // handles percent-decoding (%20, multi-byte UTF-8) and host parts.
    let all_uris = lines.iter().all(|l| l.starts_with("file://"));
    let candidates: Option<Vec<PathBuf>> = if all_uris {
        lines
            .iter()
            .map(|l| url::Url::parse(l).ok().and_then(|u| u.to_file_path().ok()))
            .collect()
    } else if lines.iter().all(|l| Path::new(l).is_absolute()) {
        Some(lines.iter().map(PathBuf::from).collect())
    } else {
        None
    };

    if let Some(paths) = candidates {
        if paths.iter().all(|p| p.exists()) {
            if paths.iter().any(|p| p.is_dir()) {
                return Err(String::from("Folders can't be shared — copy files instead"));
            }
            return Ok(ResolvedPaste {
                paths: paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
                is_text: false,
            });
        }
        if all_uris {
            // A stale file-manager copy: sharing the URIs as a .txt would
            // never be what was meant.
            return Err(String::from("Copied file(s) no longer exist"));
        }
        // Bare absolute paths that don't all exist: probably prose.
    }

    // Text branch: write the clipboard verbatim to a temp .txt and share that.
    // Timestamped name so a second paste never clobbers a file an earlier
    // (async) transfer may still be reading; the OS cleans the temp dir.
    let dir = std::env::temp_dir().join("open-quickshare");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {dir:?}: {e}"))?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = dir.join(format!("Pasted text {millis}.txt"));
    std::fs::write(&path, text.as_bytes())
        .map_err(|e| format!("Couldn't write the text file: {e}"))?;

    Ok(ResolvedPaste {
        paths: vec![path.to_string_lossy().into_owned()],
        is_text: true,
    })
}
