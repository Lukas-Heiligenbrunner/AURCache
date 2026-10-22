//! How many builds a worker runs at once, as a limit that can change while
//! builds are running.
//!
//! A `Semaphore` sized at startup cannot be resized downwards in place: with
//! every permit held, `forget_permits` removes none, and each build that
//! finishes hands its permit back and restores the old capacity. So the limit
//! is a target and a count instead. Lowering it lets the builds already running
//! finish and starts nothing until the count is under the new target; raising
//! it lets the waiting claim through at once.

use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// A resizable limit on builds running at once.
#[derive(Debug)]
pub struct ConcurrencyGate {
    state: Mutex<State>,
    /// Woken whenever a slot may have opened: a build finished, or the target
    /// went up.
    changed: Notify,
}

#[derive(Debug)]
struct State {
    target: usize,
    running: usize,
}

/// One build's place under the limit, given back when dropped -- including by
/// a build task that panicked.
#[derive(Debug)]
pub struct GatePermit {
    gate: Arc<ConcurrencyGate>,
}

impl ConcurrencyGate {
    /// A gate letting `target` builds run at once, never fewer than one: a
    /// worker that ran no builds at all would be a machine silently doing
    /// nothing.
    #[must_use]
    pub fn new(target: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                target: target.max(1),
                running: 0,
            }),
            changed: Notify::new(),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock means a panic while holding two integers; the
        // integers are still the best account of what is running.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wait for a place under the limit.
    pub async fn acquire(self: &Arc<Self>) -> GatePermit {
        loop {
            // Registered before checking, so a release between the check and
            // the wait is not missed.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state();
                if state.running < state.target {
                    state.running += 1;
                    return GatePermit {
                        gate: Arc::clone(self),
                    };
                }
            }
            notified.await;
        }
    }

    /// Change the limit. Builds already running are never stopped by a lower
    /// one; they finish, and nothing new starts until fewer than `target` run.
    pub fn set_target(&self, target: usize) {
        let raised = {
            let mut state = self.state();
            let raised = target > state.target;
            state.target = target.max(1);
            raised
        };
        if raised {
            self.changed.notify_waiters();
        }
    }

    /// The limit as it stands.
    #[must_use]
    pub fn target(&self) -> usize {
        self.state().target
    }

    /// How many builds hold a place now.
    #[must_use]
    pub fn running(&self) -> usize {
        self.state().running
    }
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        {
            let mut state = self.gate.state();
            state.running = state.running.saturating_sub(1);
        }
        self.gate.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Whether a claim would get through right now.
    async fn admits(gate: &Arc<ConcurrencyGate>) -> Option<GatePermit> {
        tokio::time::timeout(Duration::from_millis(50), gate.acquire())
            .await
            .ok()
    }

    #[tokio::test]
    async fn the_target_bounds_what_runs() {
        let gate = ConcurrencyGate::new(2);
        let _a = admits(&gate).await.expect("first");
        let _b = admits(&gate).await.expect("second");
        assert!(
            admits(&gate).await.is_none(),
            "a third got past a limit of 2"
        );
    }

    /// The case a semaphore gets wrong: lowering the limit while every place
    /// is held. The running builds carry on, and giving one back does not
    /// quietly restore the old limit.
    #[tokio::test]
    async fn lowering_lets_running_builds_finish_and_holds_new_ones() {
        let gate = ConcurrencyGate::new(3);
        let a = admits(&gate).await.unwrap();
        let b = admits(&gate).await.unwrap();
        let c = admits(&gate).await.unwrap();

        gate.set_target(1);
        assert_eq!(gate.running(), 3, "lowering stopped a running build");
        drop(a);
        assert!(admits(&gate).await.is_none(), "2 running, limit 1");
        drop(b);
        assert!(admits(&gate).await.is_none(), "1 running, limit 1");
        drop(c);
        let _d = admits(&gate).await.expect("none running, limit 1");
        assert!(admits(&gate).await.is_none());
    }

    /// Raising lets a claim that is already waiting through, without waiting
    /// for a build to finish first.
    #[tokio::test]
    async fn raising_admits_a_waiting_claim_at_once() {
        let gate = ConcurrencyGate::new(1);
        let _held = admits(&gate).await.unwrap();
        let waiting = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.acquire().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        gate.set_target(2);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the waiting claim was not let through")
            .unwrap();
    }

    /// Zero would be a worker that never builds anything; it is taken as one.
    #[tokio::test]
    async fn the_target_is_never_below_one() {
        let gate = ConcurrencyGate::new(0);
        assert_eq!(gate.target(), 1);
        gate.set_target(0);
        assert_eq!(gate.target(), 1);
        let _a = admits(&gate)
            .await
            .expect("a limit of zero admitted nothing");
    }
}
