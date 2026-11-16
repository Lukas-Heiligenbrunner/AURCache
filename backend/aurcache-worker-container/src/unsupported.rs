//! What a job may ask for that an agent-run build does not do: a persistent
//! build directory. The build goes ahead in a fresh tree, and says so.

use aurcache_common::worker::JobDescriptor;
use aurcache_worker_core::client::WorkerClient;

/// Say, in the build log and to the server, what this job asked for that an
/// agent-run build does not do. Nothing, for a job that asked for none of it.
pub async fn warn(client: &WorkerClient, job: &JobDescriptor) {
    // WP3 writes this.
    let _ = (client, job);
    unimplemented!()
}
