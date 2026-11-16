//! What the pacman helper accepts from makepkg, and what root runs for it.
//!
//! makepkg installs a build's missing dependencies through `PACMAN_AUTH`,
//! which the agent points at its own `pacman-request` subcommand. Root accepts
//! `pacman [--noconfirm|--noprogressbar|--needed|--color X] -S --asdeps
//! <targets>` and nothing else, and runs a command line it builds from the
//! validated targets alone. The grammar is in `kubernetes-executor.md`
//! ("Installing what makepkg asks for").

/// What root will install: validated targets, nothing of the caller's argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub targets: Vec<String>,
}

/// Why a request was refused: one variant per reason, whose `Display` names
/// the offending word.
// The variants and their `Display` are WP1's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {}

/// `argv` is what makepkg ran after `PACMAN_AUTH`: pacman's path first.
pub fn validate(argv: &[String]) -> Result<Install, Refusal> {
    // WP1 writes this.
    let _ = argv;
    unimplemented!()
}

/// The command line root runs for it.
#[must_use]
pub fn command(install: &Install) -> Vec<String> {
    // WP1 writes this.
    let _ = install;
    unimplemented!()
}
