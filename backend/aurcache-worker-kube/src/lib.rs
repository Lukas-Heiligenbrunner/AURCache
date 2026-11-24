//! The Kubernetes worker: one approved identity that builds each job in a
//! short-lived pod of its own.
//!
//! The dispatcher is the worker, from the server's point of view. For each job
//! it renders a pod ([`pod`]) from its configuration ([`config`],
//! [`settings`]) and sees the build through in it ([`executor`]): it hands
//! the job in, follows the log and streams the packages from the pod to the
//! server ([`transfer`]), and turns how the pod ended into the report
//! ([`outcome`]). Everything it asks of Kubernetes goes through one
//! trait, [`cluster::Cluster`], so that lifecycle is tested against a fake;
//! [`kube_cluster`] is that trait over a real API server.
//!
//! See `design/in-progress/kubernetes-executor.md`.

pub mod cluster;
pub mod config;
pub mod executor;
pub mod kube_cluster;
pub mod outcome;
pub mod pod;
pub mod settings;
pub mod transfer;

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock `mutex`, even when a panic poisoned it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
