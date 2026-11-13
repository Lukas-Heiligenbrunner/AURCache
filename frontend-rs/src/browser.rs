//! The browser window, the one way into the page's global scope.
//!
//! Off wasm there is no window, and asking for one is not merely a panic:
//! wasm-bindgen keeps the global object in a `static` it only makes sound for a
//! single-threaded wasm module, so two host tests asking at once race on its
//! initialisation and the test binary segfaults. Host builds answer `None`
//! without touching it, which is what every caller already handles.

/// The page's window, or `None` where there is none (host builds and tests).
#[cfg(target_arch = "wasm32")]
#[allow(clippy::disallowed_methods)] // The one sanctioned call.
pub fn window() -> Option<web_sys::Window> {
    web_sys::window()
}

/// The page's window, or `None` where there is none (host builds and tests).
#[cfg(not(target_arch = "wasm32"))]
pub fn window() -> Option<web_sys::Window> {
    None
}

#[cfg(test)]
mod tests {
    /// Many threads at once is how the race used to show up; now it is the
    /// plain answer from each of them.
    #[test]
    fn a_host_build_has_no_window_from_any_thread() {
        let threads: Vec<_> = (0..16).map(|_| std::thread::spawn(super::window)).collect();
        for thread in threads {
            assert!(thread.join().unwrap().is_none());
        }
    }
}
