//! The browser's `localStorage`, where per-browser display preferences live.
//!
//! Every failure reads as "nothing stored": a browser that refuses storage (a
//! private window, a policy) still gets a working page, only without the
//! preference surviving a reload.

/// The value stored under `key`, if there is one.
pub fn get(key: &str) -> Option<String> {
    crate::browser::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(key).ok().flatten())
}

/// Store `value` under `key`.
pub fn set(key: &str, value: &str) {
    if let Some(storage) = crate::browser::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(key, value);
    }
}
