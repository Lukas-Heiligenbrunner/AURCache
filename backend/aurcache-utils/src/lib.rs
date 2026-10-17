/// Telling the logs about builds taken back from their workers.
pub mod abandoned;
pub mod aur;
pub mod build_logger;
pub mod cancel;
pub mod dump;
pub mod git;
pub mod job_config;
pub mod package;
pub mod patch;
pub mod pkg;
pub mod pkgbuild;
pub mod publish;
pub mod repository;
pub mod restore;
pub mod scheduled;
pub mod services;
pub mod settings;
pub mod snapshot;
pub mod vcs_check;
pub mod worker_complete;
/// How the server polices remote workers, read once from its environment.
pub mod worker_policy;
