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

pub fn limits_from_env() -> (u64, i64) {
    // cpu_limit in milli cpus
    let cpu_limit = env::var("CPU_LIMIT")
        .ok()
        .and_then(|x| x.parse::<u64>().ok())
        .map_or(0, |x| x * 1_000_000);
    debug!("cpu_limit: {cpu_limit} mCPUs");
    // memory_limit in megabytes
    let memory_limit = env::var("MEMORY_LIMIT")
        .ok()
        .and_then(|x| x.parse::<i64>().ok())
        .map_or(-1, |x| x * 1024 * 1024);
    debug!("memory_limit: {memory_limit}MB");
    (cpu_limit, memory_limit)
}
