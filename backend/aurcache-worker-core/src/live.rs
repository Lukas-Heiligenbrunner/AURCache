//! A value that is replaced whole while it is in use.

use std::sync::{Arc, PoisonError, RwLock};

/// A configuration in force, replaced whole when the server delivers values.
///
/// Readers take the one current with [`Self::current`] and keep it: a loop
/// iteration or a build works with one copy from start to end, so nothing is
/// sized by one value and timed by another.
#[derive(Debug)]
pub struct Live<T>(RwLock<Arc<T>>);

impl<T> Live<T> {
    #[must_use]
    pub fn new(value: T) -> Self {
        Self(RwLock::new(Arc::new(value)))
    }

    /// The value in force now.
    #[must_use]
    pub fn current(&self) -> Arc<T> {
        // Poisoned only by a panic mid-assignment of an `Arc`, which leaves
        // either the old value or the new one -- both usable.
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Put `value` in force; whoever holds the previous one keeps it.
    pub fn replace(&self, value: T) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(value);
    }
}
