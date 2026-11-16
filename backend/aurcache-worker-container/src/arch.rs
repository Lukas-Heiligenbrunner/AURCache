//! Arch Linux architectures in the vocabulary of container images.

/// An Arch architecture in the OCI vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    pub arch: &'static str,
    pub variant: Option<&'static str>,
}

/// The platform for an Arch architecture name; `None` for one with no OCI
/// name.
#[must_use]
pub fn platform(arch: &str) -> Option<Platform> {
    // WP3 writes this.
    let _ = arch;
    unimplemented!()
}
