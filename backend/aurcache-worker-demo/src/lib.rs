//! AURCache demo build worker.
//!
//! Claims jobs through the normal worker protocol but produces synthetic,
//! near-empty package archives instead of compiling anything. For public demo
//! instances that show off the UI and dependency resolution without spending
//! any build resources.
//!
//! The worker protocol itself — identity, enrollment, claiming, heartbeats —
//! lives in `aurcache-worker-core`; this crate is only the [`DemoExecutor`]
//! plus this binary's thin `main`.

pub mod executor;

pub use executor::DemoExecutor;
