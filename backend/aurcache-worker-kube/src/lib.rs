//! The Kubernetes worker: one approved identity that builds each job in a
//! short-lived pod of its own.
//!
//! The dispatcher is the worker, from the server's point of view. For each job
//! it renders a pod ([`pod`]) from its configuration ([`config`],
//! [`settings`]), creates it, hands the job in, follows the log, streams the
//! packages from the pod to the server, and turns how the pod ended into the
//! report ([`executor`]). Everything it asks of Kubernetes goes through one
//! trait, [`cluster::Cluster`], so that lifecycle is tested against a fake;
//! [`kube_cluster`] is that trait over a real API server. [`probe`] checks at
//! startup that the builds' NetworkPolicy is enforced.
//!
//! See `design/in-progress/kubernetes-executor.md`.

pub mod cluster;
pub mod config;
pub mod executor;
pub mod kube_cluster;
pub mod outcome;
pub mod pod;
pub mod probe;
pub mod settings;
pub mod transfer;
