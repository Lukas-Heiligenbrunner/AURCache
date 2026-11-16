//! Which Linux capabilities this worker grants a build that asks for them.
//!
//! A job names the capabilities its package wants (`JobDescriptor::
//! capabilities`); the operator names the ones this machine allows
//! (`WORKER_BUILD_CAPABILITIES`). A build runs with what it asked for only
//! when every one of them is allowed, and is failed otherwise, naming what was
//! missing: running it without would only fail it later, further from the
//! reason.
//!
//! The allowance is the machine's alone, read from its environment and never
//! declared as a setting, for the reason `WORKER_BIND_MOUNTS` is not: it
//! decides what a build may reach, and a server that could set it could hand
//! any package any capability. See `design/implemented/build-capabilities.md`.

use aurcache_common::capability::Capabilities;

/// The environment variable naming the capabilities builds may be granted.
pub const ALLOWED_ENV: &str = "WORKER_BUILD_CAPABILITIES";

/// The capabilities `raw` (the value of [`ALLOWED_ENV`]) allows.
///
/// # Errors
/// A name that is not a capability, or one that no build may hold
/// ([`aurcache_common::capability::Capability::breaks_confinement`]). Refused
/// rather than dropped: an operator who wrote it meant something by it, and
/// would otherwise learn otherwise only from a build failing.
pub fn allowed(raw: &str) -> anyhow::Result<Capabilities> {
    let caps: Capabilities = raw
        .parse()
        .map_err(|e| anyhow::anyhow!("{ALLOWED_ENV}: {e}"))?;
    if let Some(cap) = caps.iter().find(|cap| cap.breaks_confinement()) {
        anyhow::bail!(
            "{ALLOWED_ENV}: {cap} cannot be granted to a build, because it reaches past \
             the build's container to this machine itself. Remove it; a package that \
             needs it needs a machine of its own."
        );
    }
    Ok(caps)
}

/// Whether a build may run with the capabilities it asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grant {
    /// It may, with these: every one it asked for, and possibly none.
    Granted(Capabilities),
    /// It may not; why, for its log and its report.
    Refused(String),
}

/// What a build asking for `requested` is granted by a worker allowing
/// `allowed`.
#[must_use]
pub fn grant(requested: &[String], allowed: &Capabilities) -> Grant {
    let requested = match Capabilities::from_names(requested) {
        Ok(caps) => caps,
        Err(e) => {
            return Grant::Refused(format!(
                "this build asks for a capability this worker does not know: {e}"
            ));
        }
    };
    let missing = requested.outside(allowed);
    if missing.is_empty() {
        return Grant::Granted(requested);
    }
    Grant::Refused(format!(
        "this build asks for {missing}, which this worker does not grant. Allow it on \
         the worker ({ALLOWED_ENV}={}), or remove it from the package's \
         build_capabilities setting.",
        allowed_with(allowed, &missing)
    ))
}

/// `allowed` with `more` added: the value that would let the build through.
fn allowed_with(allowed: &Capabilities, more: &Capabilities) -> Capabilities {
    allowed.iter().chain(more.iter()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(s: &str) -> Capabilities {
        s.parse().unwrap()
    }

    fn names(s: &[&str]) -> Vec<String> {
        s.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn nothing_is_allowed_unless_named() {
        assert!(allowed("").unwrap().is_empty());
        assert_eq!(
            allowed("net_admin, CAP_NET_RAW").unwrap(),
            caps("CAP_NET_ADMIN,CAP_NET_RAW")
        );
    }

    #[test]
    fn an_allowance_that_breaks_confinement_is_refused() {
        let err = allowed("net_admin,sys_admin").unwrap_err().to_string();
        assert!(err.contains("CAP_SYS_ADMIN"), "{err}");
        assert!(err.contains(ALLOWED_ENV), "{err}");
        let err = allowed("net_admn").unwrap_err().to_string();
        assert!(
            err.contains("net_admn") && err.contains(ALLOWED_ENV),
            "{err}"
        );
    }

    #[test]
    fn a_build_asking_for_nothing_is_granted_nothing() {
        assert_eq!(
            grant(&[], &Capabilities::none()),
            Grant::Granted(Capabilities::none())
        );
        assert_eq!(
            grant(&[], &caps("net_admin")),
            Grant::Granted(Capabilities::none())
        );
    }

    #[test]
    fn a_build_is_granted_what_it_asks_for_when_all_is_allowed() {
        assert_eq!(
            grant(&names(&["CAP_NET_ADMIN"]), &caps("net_admin,net_raw")),
            Grant::Granted(caps("net_admin"))
        );
    }

    /// The refusal names what is missing and the value that would allow it,
    /// keeping what the worker already allows.
    #[test]
    fn a_build_asking_for_more_is_refused_by_name() {
        let Grant::Refused(reason) = grant(
            &names(&["CAP_NET_ADMIN", "CAP_NET_RAW"]),
            &caps("net_raw,sys_time"),
        ) else {
            panic!("granted");
        };
        assert!(reason.contains("asks for CAP_NET_ADMIN, which"), "{reason}");
        assert!(
            reason.contains("WORKER_BUILD_CAPABILITIES=CAP_NET_ADMIN,CAP_NET_RAW,CAP_SYS_TIME"),
            "{reason}"
        );
        assert!(reason.contains("build_capabilities"), "{reason}");
    }

    /// A name from a newer server is refused, never ignored: building without
    /// it is not what was asked.
    #[test]
    fn a_capability_this_worker_does_not_know_is_refused() {
        let Grant::Refused(reason) = grant(&names(&["CAP_FROM_THE_FUTURE"]), &caps("net_admin"))
        else {
            panic!("granted");
        };
        assert!(reason.contains("CAP_FROM_THE_FUTURE"), "{reason}");
    }
}
