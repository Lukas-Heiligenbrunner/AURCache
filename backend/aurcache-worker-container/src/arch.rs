//! Arch Linux architectures in the vocabulary of container images.

/// The architectures that the server builds for and that the container
/// images are published for: `pacman_mirrors::Platform::ALL`, spelled out
/// because that crate would bring a database driver with it.
pub const ARCHES: [&str; 3] = ["x86_64", "aarch64", "armv7h"];

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
    let (arch, variant) = match arch {
        "x86_64" => ("amd64", None),
        "aarch64" => ("arm64", None),
        "armv7h" => ("arm", Some("v7")),
        "riscv64" => ("riscv64", None),
        _ => return None,
    };
    Some(Platform { arch, variant })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(arch: &'static str, variant: Option<&'static str>) -> Platform {
        Platform { arch, variant }
    }

    #[test]
    fn the_four_architectures_have_a_platform() {
        assert_eq!(platform("x86_64"), Some(named("amd64", None)));
        assert_eq!(platform("aarch64"), Some(named("arm64", None)));
        assert_eq!(platform("armv7h"), Some(named("arm", Some("v7"))));
        assert_eq!(platform("riscv64"), Some(named("riscv64", None)));
    }

    /// Building for the wrong architecture is worse than refusing the job.
    #[test]
    fn an_unknown_architecture_has_none() {
        assert_eq!(platform("i686"), None);
        assert_eq!(platform("amd64"), None);
        assert_eq!(platform(""), None);
    }
}
