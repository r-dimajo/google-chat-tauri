//! Linux only: files dropped into the webview, served to the page.
//!
//! The DOM `drop` event WebKit synthesizes carries `text/uri-list` as a
//! string and `files` empty (same pasteboard gap as paste -- see
//! `features::clipboard`), so a page cannot read a dropped file, and Chat
//! shows the raw `file://` path pasted into the compose box instead. But the
//! drop *is* GTK data: this module connects to the webview widget's
//! `drag-data-received` -- which wry no longer does for us, its handler being
//! disabled (see `features::window`) -- reads the URI list there, reads the
//! files, and tells the page about them.
//!
//! Why tokens. A command that takes a path would let the page read any file
//! on disk. Instead each drop registers its files under a random token, the
//! event carries names plus tokens, and `dropped_file` serves bytes only for
//! the token of the most recent drop -- so what the page can reach is exactly
//! what the user just dropped into it, and a made-up path reads nothing.
//!
//! The page-side half lives in `chat.js`: it stops WebKit's default insertion
//! of the path, and on this event dispatches a paste carrying the files at
//! the element under the pointer -- Chat attaches pasted files, so the drop
//! lands as the attachment it looks like.
//!
//! Disk reads happen on worker threads; the GTK signal handler only notes the
//! URIs and returns, so the one thread that draws the titlebar (see
//! `features::notifications`) does no disk work here.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::features::clipboard::{ClipboardFile, encode, mime_for};
use crate::features::window::MAIN;

/// The event that carries a drop's files to the page.
const DROPPED_EVENT: &str = "dropped-files";

/// The largest file this will hand over, in bytes. Same cap and reason as
/// `features::clipboard`.
const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;

/// The most files one drop may carry.
///
/// A file manager drag is a handful; a directory dragged in is not a paste
/// this replay is for.
const MAX_FILES: usize = 16;

/// How long a token outlives its drop. The page turns the event around at
/// once; the window exists for a replay that lost the race against a page
/// reload.
const TOKEN_TTL: Duration = Duration::from_secs(60);

/// A dropped file, as registered: the path to read it from when the page
/// asks, and what to tell the page about it.
struct PendingFile {
    path: PathBuf,
    name: String,
    size: u64,
}

/// One drop's files, waiting for the page to ask for the bytes.
struct Pending {
    token: String,
    files: Vec<PendingFile>,
    at: Instant,
}

/// The latest drop, if any. Replaced wholesale by every new one: an older
/// token stops working the moment a newer drop lands, and nothing needs
/// cleaning up in between -- the entries are paths, not bytes.
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

static TOKENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What the event tells the page: enough to ask, nothing to read.
#[derive(serde::Serialize, Clone)]
pub struct DroppedFile {
    pub token: String,
    pub name: String,
    pub size: u64,
}

/// Connect the GTK drag handler. Call once from setup, after the window is
/// built.
pub fn install(app: &AppHandle, window: &WebviewWindow) {
    let app = app.clone();
    let asked = window.with_webview(move |webview| {
        use gtk::prelude::*;
        webview
            .inner()
            .connect_drag_data_received(move |_view, _ctx, _x, _y, data, info, _| {
                // Same target id wry matched: the URI list.
                if info != 2 {
                    return;
                }
                let uris: Vec<String> = data.uris().into_iter().map(|u| u.to_string()).collect();
                if uris.is_empty() {
                    return;
                }

                // Off the GTK thread: disk reads and encoding are not
                // main-thread work. The event goes out from that thread too;
                // one worker keeps a burst of drops arriving in order.
                let app = app.clone();
                let spawned = std::thread::Builder::new()
                    .name("dropped-files".into())
                    .spawn(move || register_and_emit(&app, &uris));
                if let Err(e) = spawned {
                    log::warn!("dnd: no thread to read a drop: {e}");
                }
            });
    });

    if let Err(e) = asked {
        log::warn!("dnd: no webview to watch for drops: {e}");
    }
}

/// Read the dropped files and tell the page. Runs on its own thread.
fn register_and_emit(app: &AppHandle, uris: &[String]) {
    if uris.len() > MAX_FILES {
        log::info!(
            "dnd: drop carries {} files, over the cap; ignored",
            uris.len()
        );
        return;
    }

    let files: Vec<PendingFile> = uris
        .iter()
        .filter_map(|uri| {
            let raw = uri.strip_prefix("file://")?;
            let path = percent_encoding::percent_decode_str(raw)
                .decode_utf8_lossy()
                .into_owned();
            let path = Path::new(&path);

            let meta = match std::fs::metadata(path) {
                Ok(meta) => meta,
                Err(e) => {
                    log::info!("dnd: skipped {}: {e}", crate::redact::path(path));
                    return None;
                }
            };
            if !meta.is_file() {
                log::info!(
                    "dnd: skipped {}: not a regular file",
                    crate::redact::path(path)
                );
                return None;
            }
            if meta.len() > MAX_FILE_BYTES as u64 {
                log::info!(
                    "dnd: skipped {}: {} MB, over the cap",
                    crate::redact::path(path),
                    meta.len() / (1024 * 1024)
                );
                return None;
            }

            let name = path.file_name()?.to_string_lossy().into_owned();
            Some(PendingFile {
                path: path.to_owned(),
                name,
                size: meta.len(),
            })
        })
        .collect();

    if files.is_empty() {
        return;
    }

    let token = format!(
        "drop-{}",
        TOKENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let listed: Vec<DroppedFile> = files
        .iter()
        .map(|f| DroppedFile {
            token: token.clone(),
            name: f.name.clone(),
            size: f.size,
        })
        .collect();

    *PENDING.lock().unwrap() = Some(Pending {
        token,
        files,
        at: Instant::now(),
    });

    if let Err(e) = app.emit_to(MAIN, DROPPED_EVENT, &listed) {
        log::error!("dnd: failed to tell the page about a drop: {e}");
    }
}

/// The bytes of one dropped file. Only a token from the latest drop answers.
///
/// Async: the read and encode belong off the main thread, which Tauri puts
/// async commands off anyway.
pub async fn dropped_file(
    app: AppHandle,
    token: String,
    index: usize,
) -> Result<Option<ClipboardFile>, String> {
    // The page the window shows decides, not the frame the call came from:
    // WebKitGTK does not say which frame that is (same as `features::media`).
    let on_chat = app
        .get_webview_window(MAIN)
        .and_then(|w| w.url().ok())
        .is_some_and(|u| crate::urls::is_chat_page(&u));
    if !on_chat {
        return Err("not on a Chat page".into());
    }

    let pending = PENDING
        .lock()
        .unwrap()
        .take_if(|p| p.token == token && Instant::now().duration_since(p.at) < TOKEN_TTL);
    let pending = pending.ok_or("token is not the latest drop")?;
    let file = pending.files.get(index).ok_or("no file at that index")?;

    let path = file.path.clone();
    let name = file.name.clone();
    let bytes = tokio::task::spawn_blocking(move || std::fs::read(&path))
        .await
        .map_err(|e| format!("drop task failed: {e}"))?
        .map_err(|e| format!("{}: {e}", crate::redact::path(&file.path)))?;
    let mime = mime_for(&name, &bytes);

    Ok(Some(ClipboardFile {
        name,
        mime,
        base64: encode(&bytes),
    }))
}
