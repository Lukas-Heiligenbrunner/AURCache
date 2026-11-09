//! A request a button starts: whether it is in flight, and how it failed.

use dioxus::prelude::*;
use std::future::Future;

/// The bookkeeping every button that calls the API repeats: disabled while its
/// request runs, and the error it ended on shown beside it.
#[derive(Clone, Copy, PartialEq)]
pub struct Action {
    busy: Signal<bool>,
    error: Signal<Option<String>>,
}

pub fn use_action() -> Action {
    Action {
        busy: use_signal(|| false),
        error: use_signal(|| None),
    }
}

impl Action {
    /// Run `request`, busy until it answers.
    ///
    /// The value on success, for the caller to act on; `None` when it failed,
    /// with the error then in [`Self::error`]. A new run clears the last error.
    pub async fn run<T>(mut self, request: impl Future<Output = Result<T, String>>) -> Option<T> {
        self.busy.set(true);
        self.error.set(None);
        let outcome = request.await;
        self.busy.set(false);
        match outcome {
            Ok(value) => Some(value),
            Err(e) => {
                self.error.set(Some(e));
                None
            }
        }
    }

    /// Whether a request is in flight.
    pub fn busy(&self) -> bool {
        (self.busy)()
    }

    /// The last request's error, until the next one starts.
    pub fn error(&self) -> Option<String> {
        (self.error)()
    }

    /// Forget the last error, as a dialog does when it is opened again.
    pub fn clear(mut self) {
        self.error.set(None);
    }
}
