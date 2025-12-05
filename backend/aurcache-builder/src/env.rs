use std::env;
use std::time::Duration;
use tracing::debug;

pub fn job_timeout_from_env() -> Duration {
    let job_timeout = env::var("JOB_TIMEOUT")
        .ok()
        .and_then(|x| x.parse::<u64>().ok())
        .map_or(Duration::from_secs(60 * 60), Duration::from_secs);
    debug!("job_timeout: {} sec", job_timeout.as_secs());
    job_timeout
}
