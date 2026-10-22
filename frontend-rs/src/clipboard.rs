//! Writing text to the clipboard, for the copy buttons.

use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

/// Why a copy did not happen.
pub enum CopyError {
    /// No clipboard: the page is not in a secure context.
    Unavailable,
    /// The browser refused the write.
    Refused,
}

impl CopyError {
    /// What to tell the user, naming what was being copied.
    pub fn message(&self, what: &str) -> String {
        match self {
            Self::Unavailable => "Clipboard is unavailable on this connection \
                                  (it needs a secure context, like https or localhost)."
                .to_string(),
            Self::Refused => format!("Could not copy {what} to the clipboard."),
        }
    }
}

/// Put `text` on the clipboard.
pub async fn copy_text(text: &str) -> Result<(), CopyError> {
    let clipboard = web_sys::window()
        .map(|w| w.navigator().clipboard())
        // Outside a secure context `navigator.clipboard` is undefined, and
        // web-sys hands that straight back as a `Clipboard` rather than `None`.
        // Calling `write_text` on it throws through the wasm boundary, which
        // takes the page down instead of showing a message -- and http on a
        // LAN address is an ordinary way to reach this UI.
        .filter(|c| !AsRef::<JsValue>::as_ref(c).is_undefined())
        .ok_or(CopyError::Unavailable)?;
    JsFuture::from(clipboard.write_text(text))
        .await
        .map(|_| ())
        .map_err(|_| CopyError::Refused)
}
