//! Linux only: what the system clipboard holds, as paste-able files.
//!
//! WebKitGTK delivers a paste of copied files or images with `files` empty --
//! on Wayland the event carries `text/uri-list` or `text/html` strings and
//! nothing else, and `getData` answers the uri-list with an empty string, so
//! the page cannot even learn what was copied (see the paste entry in
//! `chat.js` and the measurements in `docs/Notes.md`). The clipboard itself
//! still knows: GTK reads the `text/uri-list` and `image/png` targets off it,
//! whatever the DOM was told.
//!
//! This is a deliberate, narrow exception to the rule that the page never
//! reads files through this app. What bounds it:
//!
//! * The ACL only answers on the Chat origins, like every other `chat-ipc`
//!   command -- but unlike the others, what is at stake is *file contents*,
//!   so the handler itself re-checks the page the window is showing.
//! * The page passes no paths at all. What comes back is exactly what the
//!   clipboard currently holds, so the page can reach only what the user last
//!   copied -- the same thing a Ctrl+V reaches in any browser.
//! * Each file is capped and the clipboard in file count; a video is big, and
//!   the page can call this in a loop. The cap is Chat's own attachment limit
//!   -- a larger file cannot be sent anyway.
//!
//! The clipboard lives on the GTK main thread: the reads happen there, via
//! [`tauri::AppHandle::run_on_main_thread`], and the file reads and base64
//! encoding move off it, so the one thread that draws the titlebar only
//! waits on the clipboard owner (see the note in `features::notifications`).

use std::path::Path;

use tauri::{AppHandle, Manager};

use crate::features::window::MAIN;

/// The largest file this will hand over, in bytes.
///
/// Chat refuses larger attachments itself, so serving one serves nothing.
const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;

/// The most files a clipboard may hold for this to serve it at all.
///
/// A file manager copy is a handful. A directory tree dragged onto the
/// clipboard is not a paste this replay is for.
const MAX_FILES: usize = 16;

/// What the page gets. `base64` because the IPC is JSON.
#[derive(serde::Serialize)]
pub struct ClipboardFile {
    pub name: String,
    pub mime: String,
    pub base64: String,
}

#[derive(serde::Serialize)]
pub struct ClipboardContent {
    /// Files whose `file://` URIs are on the clipboard's `text/uri-list`.
    pub files: Vec<ClipboardFile>,
    /// The clipboard's `image/png`, when it holds one -- a copied image or a
    /// screenshot, named for what it is; Chat lets the user rename it.
    pub image: Option<ClipboardFile>,
}

/// Read the clipboard and answer with whatever is paste-able in it.
pub async fn read(app: AppHandle) -> Result<ClipboardContent, String> {
    // The page the window shows decides, not the frame the call came from:
    // WebKitGTK does not say which frame that is (same as `features::media`).
    let on_chat = app
        .get_webview_window(MAIN)
        .and_then(|w| w.url().ok())
        .is_some_and(|u| crate::urls::is_chat_page(&u));
    if !on_chat {
        return Err("not on a Chat page".into());
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(read_clipboard_on_main());
    })
    .map_err(|e| format!("main thread unavailable: {e}"))?;

    let raw = rx.await.map_err(|e| e.to_string())?;
    if raw.uris.len() > MAX_FILES {
        return Err(format!(
            "clipboard holds {} files, more than the {} this serves",
            raw.uris.len(),
            MAX_FILES
        ));
    }

    // Disk reads and encoding off the main thread; the GTK reads are done.
    let uris = raw.uris;
    let png = raw.png;
    let files = tokio::task::spawn_blocking(move || read_files(&uris))
        .await
        .map_err(|e| format!("clipboard task failed: {e}"))?;

    let image = png.and_then(|bytes| {
        if bytes.len() > MAX_FILE_BYTES {
            log::warn!(
                "clipboard: image is {} MB, over the cap; dropped",
                bytes.len() / (1024 * 1024)
            );
            None
        } else {
            Some(ClipboardFile {
                name: "image.png".into(),
                mime: "image/png".into(),
                base64: encode(&bytes),
            })
        }
    });

    Ok(ClipboardContent { files, image })
}

/// What the GTK main thread reads: the raw clipboard answers.
struct Raw {
    uris: Vec<String>,
    png: Option<Vec<u8>>,
}

#[cfg(target_os = "linux")]
fn read_clipboard_on_main() -> Raw {
    let display = match gtk::gdk::Display::default() {
        Some(display) => display,
        None => {
            return Raw {
                uris: Vec::new(),
                png: None,
            };
        }
    };
    let Some(clipboard) = gtk::Clipboard::default(&display) else {
        return Raw {
            uris: Vec::new(),
            png: None,
        };
    };

    let uris = clipboard
        .wait_for_contents(&gtk::gdk::Atom::intern("text/uri-list"))
        .map(|data| data.uris().into_iter().map(|u| u.to_string()).collect())
        .unwrap_or_default();

    // Only when the clipboard really holds an image: Chrome's copy-image, a
    // screenshot, an image file copied by "Copy Image" from a manager.
    let png = clipboard
        .wait_for_contents(&gtk::gdk::Atom::intern("image/png"))
        .map(|data| data.data());

    Raw { uris, png }
}

/// Turn the clipboard's `file://` URIs into paste-able files.
///
/// Off-thread. URIs that are not files (a dragged link), directories and
/// over-cap files are skipped rather than refused, so one bad entry does not
/// cost the rest of the paste.
#[cfg(target_os = "linux")]
fn read_files(uris: &[String]) -> Vec<ClipboardFile> {
    let mut files = Vec::new();
    for uri in uris {
        if let Err(reason) = read_file(uri).and_then(|f| {
            if f.base64.is_empty() {
                Err("empty".into())
            } else {
                files.push(f);
                Ok(())
            }
        }) {
            log::info!("clipboard: skipped a file: {reason}");
        }
    }
    files
}

/// Read one `file://` URI, capped and typed.
#[cfg(target_os = "linux")]
fn read_file(uri: &str) -> Result<ClipboardFile, String> {
    let raw = uri
        .strip_prefix("file://")
        .ok_or_else(|| "not a file uri".to_string())?;

    let path = percent_encoding::percent_decode_str(raw)
        .decode_utf8_lossy()
        .into_owned();
    let path = Path::new(&path);

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or("no file name in the uri")?;

    let meta =
        std::fs::metadata(path).map_err(|e| format!("{}: {e}", crate::redact::path(path)))?;
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular file",
            crate::redact::path(path)
        ));
    }
    if meta.len() as usize > MAX_FILE_BYTES {
        return Err(format!(
            "{} is {} MB, over the cap",
            crate::redact::path(path),
            meta.len() / (1024 * 1024)
        ));
    }

    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", crate::redact::path(path)))?;
    let mime = mime_for(&name, &bytes);
    Ok(ClipboardFile {
        name,
        mime,
        base64: encode(&bytes),
    })
}

pub(crate) fn encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A MIME type for the file, by extension first and sniffing second.
///
/// Chat needs the type to route the attachment; a video that arrives as
/// `application/octet-stream` still attaches but loses its inline preview.
pub(crate) fn mime_for(name: &str, bytes: &[u8]) -> String {
    let mime = by_extension(name);
    if mime != "application/octet-stream" {
        return mime;
    }
    // Sniff the common cases Chat cares about. MP4 starts with a box size
    // then `ftyp`; WebM is an EBML header.
    if bytes.len() > 12 && &bytes[4..8] == b"ftyp" {
        return "video/mp4".into();
    }
    if bytes.len() > 4 && bytes[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        return "video/webm".into();
    }
    mime
}

fn by_extension(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "txt" | "md" => "text/plain",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
    .to_string()
}
