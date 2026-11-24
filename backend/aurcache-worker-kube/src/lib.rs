//! The Kubernetes worker: one approved identity that builds each job in a
//! short-lived pod of its own.
//!
//! The dispatcher is the worker, from the server's point of view. For each job
//! it renders a pod ([`pod`]) from its configuration ([`config`],
//! [`settings`]), and turns how the pod ended into the report ([`outcome`]).
//!
//! See `design/in-progress/kubernetes-executor.md`.

pub mod config;
pub mod outcome;
pub mod pod;
pub mod settings;
