//! Debug-only probe for the paste and drag-and-drop paths.
//!
//! Nothing here ships: the whole module is `#[cfg(all(debug_assertions, target_os = "linux"))]`
//! and gated behind `GOOGLE_CHAT_PASTE_PROBE=1`. It evals a script onto the real
//! Chat page that reports every paste / drag event with the full contents of
//! its `clipboardData` / `dataTransfer`, so the platform's actual behaviour can
//! be read out of the log rather than guessed at.
//!
//! Findings so far are in `docs/Notes.md` under the paste entry.

use tauri::WebviewWindow;

pub const ENV_VAR: &str = "GOOGLE_CHAT_PASTE_PROBE";

pub fn enabled() -> bool {
    cfg!(all(debug_assertions, target_os = "linux"))
        && std::env::var(ENV_VAR).map(|v| v != "0").unwrap_or(false)
}

const PROBE_JS: &str = r#"
(function () {
  if (window.__gchat_probe) return;
  window.__gchat_probe = true;

  const tauriInvoke =
    (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) ||
    (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
  if (!tauriInvoke) return;

  const send = (msg) =>
    tauriInvoke('page_log', { level: 'info', message: `[probe] ${msg}` }).catch(() => {});

  // Describe a DataTransfer as compactly as its contents allow.
  function describe(dt) {
    if (!dt) return 'no-dataTransfer';
    const parts = [];
    try {
      parts.push(`types=[${dt.types.join(',')}]`);
    } catch (e) {}
    try {
      const files = [];
      for (const f of dt.files) files.push(`${f.name}:${f.type}:${f.size}B`);
      parts.push(`files(${dt.files.length})=[${files.join('|')}]`);
    } catch (e) {
      parts.push('files=<error>');
    }
    try {
      // What getData actually answers per type -- the pasteboard gap in one
      // line. Done here rather than in chat.js so the page's own logic is
      // untouched; the probe is the instrument.
      const got = [];
      for (const t of dt.types) {
        let v;
        try {
          v = dt.getData(t);
          got.push(`${t}=${v.length}B`);
        } catch (err) {
          got.push(`${t}=throw`);
        }
      }
      parts.push(`getData{${got.join(',')}}`);
    } catch (e) {
      parts.push('getData=<error>');
    }
    return parts.join(' ');
  }

  function describeEvent(name, event) {
    const dt = event.clipboardData || event.dataTransfer;
    send(`${name} trusted=${event.isTrusted} default-prevented=${event.defaultPrevented} target=${(event.target && event.target.tagName) || '?'} ${describe(dt)}`);
  }

  for (const name of ['paste', 'dragenter', 'dragover', 'drop', 'dragleave']) {
    window.addEventListener(
      name,
      (event) => describeEvent(name, event),
      true
    );
  }

  // What the async clipboard offers right now, on demand (F5 on the page).
  window.addEventListener(
    'keydown',
    (event) => {
      if (event.key !== 'F5' || !navigator.clipboard || !navigator.clipboard.read) return;
      navigator.clipboard
        .read()
        .then((items) => {
          for (const item of items) {
            for (const type of item.types) {
              if (type.startsWith('image/') || type === 'text/html' || type === 'text/plain') {
                item
                  .getType(type)
                  .then((blob) =>
                    send(`clipboard.read: ${type} ${blob.size}B (${blob.type})`)
                  )
                  .catch((err) => send(`clipboard.read: ${type} read failed: ${err.name}`));
              } else {
                send(`clipboard.read: offers ${type} (not fetched)`);
              }
            }
          }
          if (!items.length) send('clipboard.read: no items');
        })
        .catch((err) => send(`clipboard.read failed: ${err.name}`));
    },
    true
  );

  send('probe installed');
})();
"#;

/// Install the probe on the window's pages. Call once from setup.
pub fn install(window: &WebviewWindow) {
    if !enabled() {
        return;
    }
    log::info!(
        "paste probe: enabled via {ENV_VAR}; paste/drag events and F5 clipboard reads report to the log"
    );
    if let Err(e) = window.eval(PROBE_JS) {
        log::error!("paste probe: failed to eval: {e}");
    }
}
