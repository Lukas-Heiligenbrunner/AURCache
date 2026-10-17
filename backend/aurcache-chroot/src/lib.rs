//! The worker's storage pool: one btrfs filesystem, with simple quotas, that
//! holds everything a worker writes -- the base chroot, each build's chroot and
//! workdir, and the caches -- so that a build's disk use is bounded by the
//! kernel while it runs, and the worker's whole footprint by one figure.
//!
//! See `design/implemented/build-disk-quota.md` for why it is shaped this way.
//!
//! The API takes plain data (build ids, sizes) and decides every path itself,
//! so that it could later move behind a socket to a privileged process without
//! being redesigned. Nothing here takes a path chosen by a caller beyond the
//! pool's own backing and mount point, which come from the operator.

mod cmd;
pub mod pool;
pub mod qgroup;
pub mod volumes;

pub use pool::{Backing, BuildUsage, BuildVolumes, KeptBuild, Owner, Pool, PoolConfig, Resize};
pub use qgroup::{QgroupId, Usage};
pub use volumes::CacheVolumes;
