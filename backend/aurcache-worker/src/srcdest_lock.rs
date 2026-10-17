//! Exclusive use of a pkgbase's `SRCDEST`, and the record of who is using one.
//!
//! One question, asked from two directions. The cache garbage-collector asks
//! "is anyone using this source cache?" so it never evicts a live one; a job
//! about to build asks "may I use it?", because `SRCDEST` is keyed by pkgbase
//! and *not* by platform -- downloads are architecture-independent, so an
//! x86_64 and an aarch64 build of one package share a single directory, and
//! two makepkg runs downloading into it at once race on the same files (see
//! the last paragraph for which ones).
//!
//! A guard answers both. Holding one grants the exclusive use *and* keeps the
//! pkgbase in the in-use set, so the two can never disagree -- which is what a
//! plain `HashSet` got wrong: two concurrent builds of one pkgbase collapsed to
//! a single entry, and the first to finish removed it, leaving the second's
//! source cache evictable while it was still building.
//!
//! **At most one holder per pkgbase, and an entry outlives its holder only
//! while someone is queued for it.** So the map is not counting users -- it
//! cannot have more than one -- it is keeping the waiters' rendezvous alive:
//! removing an entry someone is queued on would mint a fresh lock for the next
//! caller and hand out the same directory twice.
//!
//! The key is the pkgbase because `SRCDEST` is (`Cache::srcdest`); two packages
//! sharing an upstream URL keep separate mirrors and cannot collide. Pooling
//! the mirrors by URL would move the key to the source and make a build take
//! several of these at once, in a fixed order -- see
//! `design/implemented/remote-workers.md`.
//!
//! What makes the exclusion necessary is the *plain* sources, not the git ones:
//! a tarball is fetched as `<file>.part` and renamed, keyed by filename, so two
//! builds of one package -- different platforms of it -- race on that name. Git
//! would be fine concurrently, since a fetch only adds objects and moves refs
//! atomically.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// The `SRCDEST` directories in use on this worker.
#[derive(Default)]
pub struct SrcdestLocks {
    /// One entry per pkgbase that some job holds or is waiting for. Removed
    /// when the last of them lets go, so the keys are exactly the live set.
    ///
    /// A `std` mutex: every critical section here is a map lookup, and holding
    /// it across an await is precisely what must not happen -- the *entry's*
    /// lock is the one a job waits on, and it is taken after this one is
    /// released.
    entries: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl SrcdestLocks {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Take exclusive use of a pkgbase's `SRCDEST`, waiting for whoever has it.
    ///
    /// Waiting rather than refusing: a job that reaches this point has already
    /// been claimed, and the server excludes a package this worker is already
    /// building when it hands out work, so a wait here means something upstream
    /// let a second one through -- a one-shot build, a bug.
    /// Waiting costs that build some of its timeout; fetching over a sibling's
    /// sources costs both builds their correctness.
    pub async fn acquire(self: &Arc<Self>, pkgbase: &str) -> SrcdestGuard {
        let entry = {
            let mut entries = self.lock_entries();
            Arc::clone(
                entries
                    .entry(pkgbase.to_string())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        };
        // Entry kept alive by this clone, so the release below cannot drop it
        // from the map while we are queued for it.
        let guard = Arc::clone(&entry).lock_owned().await;
        SrcdestGuard {
            locks: Arc::clone(self),
            pkgbase: pkgbase.to_string(),
            entry,
            guard: Some(guard),
        }
    }

    /// Every pkgbase currently spoken for, which is what the cache
    /// garbage-collector must not evict.
    #[must_use]
    pub fn in_use(&self) -> Vec<String> {
        self.lock_entries().keys().cloned().collect()
    }

    /// A poisoned lock here means a thread panicked holding a map lookup;
    /// the map itself is still consistent, so carry on with it rather than
    /// bringing the worker down over a set of strings.
    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<AsyncMutex<()>>>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Exclusive use of one pkgbase's `SRCDEST`, held for as long as the build --
/// and anything that reads what the build used, such as the commit a VCS
/// source was at -- still needs the directory to hold still.
pub struct SrcdestGuard {
    locks: Arc<SrcdestLocks>,
    pkgbase: String,
    entry: Arc<AsyncMutex<()>>,
    /// `Option` only so `Drop` can release the exclusion before deciding
    /// whether the entry is still wanted.
    guard: Option<OwnedMutexGuard<()>>,
}

impl Drop for SrcdestGuard {
    fn drop(&mut self) {
        // Order matters: let go of the lock first, so a waiter can take it, and
        // only then ask whether anyone still holds a reference. Asking first
        // would count ourselves and keep the entry forever.
        drop(self.guard.take());
        let mut entries = self.locks.lock_entries();
        // Two references mean the map's and ours; anyone waiting for this
        // pkgbase holds one too, and their entry must survive for them to be
        // handed the lock.
        if Arc::strong_count(&self.entry) <= 2 {
            entries.remove(&self.pkgbase);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The in-use set is exactly what is held: a guard puts its pkgbase in it,
    /// and dropping the last guard takes it out.
    #[tokio::test]
    async fn a_held_pkgbase_is_in_use_and_a_released_one_is_not() {
        let locks = SrcdestLocks::new();
        assert!(locks.in_use().is_empty());

        let guard = locks.acquire("fonts-git").await;
        assert_eq!(locks.in_use(), vec!["fonts-git".to_string()]);

        drop(guard);
        assert!(locks.in_use().is_empty());
    }

    /// The bug the `HashSet` had: with two builds of one pkgbase, the first to
    /// finish must not take the pkgbase out from under the second.
    #[tokio::test]
    async fn the_first_of_two_finishers_leaves_the_pkgbase_in_use() {
        let locks = SrcdestLocks::new();
        let first = locks.acquire("fonts-git").await;

        // A second build of the same pkgbase, queued behind the first.
        let mut waiting = {
            let locks = Arc::clone(&locks);
            tokio::spawn(async move { locks.acquire("fonts-git").await })
        };
        // Given real time to finish, and it must not. A bare `is_finished`
        // after a yield would pass just as readily for a task that had not been
        // scheduled yet -- which is what a lock-less version looks like.
        let too_soon = tokio::time::timeout(Duration::from_millis(100), &mut waiting).await;
        assert!(
            too_soon.is_err(),
            "the second build must wait for the first to let go"
        );
        assert_eq!(locks.in_use(), vec!["fonts-git".to_string()]);

        drop(first);
        let second = waiting.await.expect("second build acquires");
        assert_eq!(
            locks.in_use(),
            vec!["fonts-git".to_string()],
            "the pkgbase is still in use, so its SRCDEST is still protected"
        );

        drop(second);
        assert!(locks.in_use().is_empty());
    }

    /// Different packages do not share a source cache and must not queue behind
    /// each other; `concurrency > 1` exists for exactly that.
    #[tokio::test]
    async fn different_pkgbases_do_not_wait_for_each_other() {
        let locks = SrcdestLocks::new();
        let _one = locks.acquire("fonts-git").await;
        let _two = locks.acquire("other-git").await;
        let mut held = locks.in_use();
        held.sort();
        assert_eq!(held, vec!["fonts-git".to_string(), "other-git".to_string()]);
    }
}
