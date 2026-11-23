//! How many jobs a build runs, as the `makepkg.conf` lines every worker
//! appends after the job's own, so that they win.
//!
//! `cpu.max` bounds the time a build gets, not how many processes it starts,
//! and nothing inside the build can see the limit: `nproc` counts the cores
//! the machine has. So `MAKEFLAGS=-j$(nproc)` under a limit starts a compiler
//! per core -- 24 of them sharing, say, six cores' worth of time, each holding
//! its own memory, which is how a CPU limit turns into an out-of-memory kill.
//! `OMP_NUM_THREADS` is what `nproc` itself, and OpenMP, defer to;
//! `CARGO_BUILD_JOBS` is cargo's own.

/// The lines for a build limited to `cpus`: a job per core when nothing
/// limits it, and as many as the CPUs, rounded up, otherwise.
#[must_use]
pub fn makepkg_lines(cpus: Option<f64>) -> String {
    let Some(cpus) = cpus else {
        return "MAKEFLAGS=\"-j$(nproc)\"\n".to_string();
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let jobs = (cpus.ceil() as u64).max(1);
    format!(
        "MAKEFLAGS=\"-j{jobs}\"\nexport OMP_NUM_THREADS={jobs}\nexport CARGO_BUILD_JOBS={jobs}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::makepkg_lines;

    #[test]
    fn parallelism_follows_the_cpu_limit() {
        assert_eq!(makepkg_lines(None), "MAKEFLAGS=\"-j$(nproc)\"\n");
        assert_eq!(
            makepkg_lines(Some(2.5)),
            "MAKEFLAGS=\"-j3\"\nexport OMP_NUM_THREADS=3\nexport CARGO_BUILD_JOBS=3\n"
        );
        for never_none in [0.5, 0.0, -3.0] {
            assert!(makepkg_lines(Some(never_none)).starts_with("MAKEFLAGS=\"-j1\"\n"));
        }
    }
}
