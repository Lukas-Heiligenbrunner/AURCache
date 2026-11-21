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
    let (arch, variant) = match arch {
        "x86_64" => ("amd64", None),
        "aarch64" => ("arm64", None),
        "armv7h" => ("arm", Some("v7")),
        "riscv64" => ("riscv64", None),
        _ => return None,
    };
    Some(Platform { arch, variant })
}

/// Refuse the architectures a worker advertises (`WORKER_ARCHES`, or the
/// machine's own) when one is not among those it `builds`: a worker that
/// runs each build in an image pulled for its platform would advertise it,
/// claim its builds and refuse every one.
///
/// # Errors
///
/// Naming each architecture it does not build.
pub fn check(arches: &[String], builds: &[&str]) -> anyhow::Result<()> {
    let unknown: Vec<&str> = arches
        .iter()
        .map(String::as_str)
        .filter(|arch| !builds.contains(arch) || platform(arch).is_none())
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "this worker does not build for {} (WORKER_ARCHES, or the machine's own when that is \
         unset); it builds for {}",
        unknown.join(", "),
        builds.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every architecture with a platform.
    const KNOWN: [&str; 4] = ["x86_64", "aarch64", "armv7h", "riscv64"];

    #[test]
    fn every_known_architecture_has_a_platform() {
        assert!(KNOWN.iter().all(|arch| platform(arch).is_some()));
        assert!(check(&KNOWN.map(String::from), &KNOWN).is_ok());
    }

    #[test]
    fn a_list_with_an_unknown_architecture_is_refused_naming_it() {
        let arches = ["x86_64", "i686", "amd64"].map(String::from);
        let refused = check(&arches, &KNOWN).unwrap_err().to_string();
        assert!(refused.contains("build for i686, amd64 ("), "{refused}");
        assert!(check(&[], &KNOWN).is_ok());
    }

    /// An architecture with a platform is still refused by a worker that
    /// does not build it.
    #[test]
    fn only_what_the_worker_builds_is_accepted() {
        let builds = ["x86_64", "aarch64"];
        let refused = check(&["riscv64".to_string()], &builds)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("build for riscv64 ("), "{refused}");
        assert!(
            refused.ends_with("it builds for x86_64, aarch64"),
            "{refused}"
        );
        assert!(check(&["aarch64".to_string()], &builds).is_ok());
    }

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
