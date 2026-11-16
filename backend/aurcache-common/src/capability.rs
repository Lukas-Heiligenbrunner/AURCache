//! Linux capabilities a build may be granted beyond what its sandbox allows.
//!
//! A package names the ones it needs (the `build_capabilities` setting), and a
//! worker grants only those its operator allows (`WORKER_BUILD_CAPABILITIES`);
//! see `design/implemented/build-capabilities.md`. Both sides read and write
//! the names the kernel documents in capabilities(7), `CAP_NET_ADMIN`, which
//! is also how they travel in a job.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

/// One Linux capability, in the kernel's numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    Chown,
    DacOverride,
    DacReadSearch,
    Fowner,
    Fsetid,
    Kill,
    Setgid,
    Setuid,
    Setpcap,
    LinuxImmutable,
    NetBindService,
    NetBroadcast,
    NetAdmin,
    NetRaw,
    IpcLock,
    IpcOwner,
    SysModule,
    SysRawio,
    SysChroot,
    SysPtrace,
    SysPacct,
    SysAdmin,
    SysBoot,
    SysNice,
    SysResource,
    SysTime,
    SysTtyConfig,
    Mknod,
    Lease,
    AuditWrite,
    AuditControl,
    Setfcap,
    MacOverride,
    MacAdmin,
    Syslog,
    WakeAlarm,
    BlockSuspend,
    AuditRead,
    Perfmon,
    Bpf,
    CheckpointRestore,
}

/// Each capability's name, indexed by its number.
const NAMES: [&str; 41] = [
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_LINUX_IMMUTABLE",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_ADMIN",
    "CAP_NET_RAW",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_SYS_MODULE",
    "CAP_SYS_RAWIO",
    "CAP_SYS_CHROOT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_PACCT",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_NICE",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG",
    "CAP_MKNOD",
    "CAP_LEASE",
    "CAP_AUDIT_WRITE",
    "CAP_AUDIT_CONTROL",
    "CAP_SETFCAP",
    "CAP_MAC_OVERRIDE",
    "CAP_MAC_ADMIN",
    "CAP_SYSLOG",
    "CAP_WAKE_ALARM",
    "CAP_BLOCK_SUSPEND",
    "CAP_AUDIT_READ",
    "CAP_PERFMON",
    "CAP_BPF",
    "CAP_CHECKPOINT_RESTORE",
];

impl Capability {
    /// Every capability, in the kernel's numbering.
    pub const ALL: [Self; 41] = [
        Self::Chown,
        Self::DacOverride,
        Self::DacReadSearch,
        Self::Fowner,
        Self::Fsetid,
        Self::Kill,
        Self::Setgid,
        Self::Setuid,
        Self::Setpcap,
        Self::LinuxImmutable,
        Self::NetBindService,
        Self::NetBroadcast,
        Self::NetAdmin,
        Self::NetRaw,
        Self::IpcLock,
        Self::IpcOwner,
        Self::SysModule,
        Self::SysRawio,
        Self::SysChroot,
        Self::SysPtrace,
        Self::SysPacct,
        Self::SysAdmin,
        Self::SysBoot,
        Self::SysNice,
        Self::SysResource,
        Self::SysTime,
        Self::SysTtyConfig,
        Self::Mknod,
        Self::Lease,
        Self::AuditWrite,
        Self::AuditControl,
        Self::Setfcap,
        Self::MacOverride,
        Self::MacAdmin,
        Self::Syslog,
        Self::WakeAlarm,
        Self::BlockSuspend,
        Self::AuditRead,
        Self::Perfmon,
        Self::Bpf,
        Self::CheckpointRestore,
    ];

    /// The name capabilities(7) gives it, `CAP_NET_ADMIN`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        NAMES[self as usize]
    }

    /// Whether holding it lets a build act on the machine beyond any
    /// container it runs in, so that no setting may grant it.
    ///
    /// These are what container runtimes withhold because each one reaches
    /// the kernel or the host directly: mounting and namespaces
    /// (`CAP_SYS_ADMIN`), loading kernel code (`CAP_SYS_MODULE`, `CAP_BPF`),
    /// raw device and port access (`CAP_SYS_RAWIO`), reading kernel memory
    /// and events (`CAP_PERFMON`), tracing other processes
    /// (`CAP_SYS_PTRACE`), opening files by handle past any mount
    /// (`CAP_DAC_READ_SEARCH`), rebooting (`CAP_SYS_BOOT`), and overriding or
    /// reconfiguring the LSM (`CAP_MAC_OVERRIDE`, `CAP_MAC_ADMIN`). A build
    /// that needs one of them needs a machine of its own, not a setting.
    #[must_use]
    pub const fn breaks_confinement(self) -> bool {
        matches!(
            self,
            Self::SysAdmin
                | Self::SysModule
                | Self::Bpf
                | Self::SysRawio
                | Self::Perfmon
                | Self::SysPtrace
                | Self::DacReadSearch
                | Self::SysBoot
                | Self::MacOverride
                | Self::MacAdmin
        )
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A name that is not a Linux capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownCapability {
    pub name: String,
}

impl fmt::Display for UnknownCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a Linux capability (expected a name such as CAP_NET_ADMIN)",
            self.name
        )
    }
}

impl std::error::Error for UnknownCapability {}

impl FromStr for Capability {
    type Err = UnknownCapability;

    /// Any case, with or without the `CAP_` prefix: `CAP_NET_ADMIN`,
    /// `net_admin` and `Net_Admin` are the same capability.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let upper = s.trim().to_ascii_uppercase();
        let bare = upper.strip_prefix("CAP_").unwrap_or(&upper);
        Self::ALL
            .into_iter()
            .find(|cap| &cap.name()["CAP_".len()..] == bare)
            .ok_or_else(|| UnknownCapability {
                name: s.trim().to_string(),
            })
    }
}

/// A set of capabilities, in the kernel's order, each named once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities(BTreeSet<Capability>);

impl Capabilities {
    /// No capability at all.
    #[must_use]
    pub const fn none() -> Self {
        Self(BTreeSet::new())
    }

    /// The capabilities `names` spell, as [`Capability::from_str`] reads them.
    ///
    /// # Errors
    /// The first name that is not a capability.
    pub fn from_names<S: AsRef<str>>(
        names: impl IntoIterator<Item = S>,
    ) -> Result<Self, UnknownCapability> {
        names
            .into_iter()
            .map(|name| name.as_ref().parse())
            .collect::<Result<BTreeSet<_>, _>>()
            .map(Self)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn contains(&self, cap: Capability) -> bool {
        self.0.contains(&cap)
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.0.iter().copied()
    }

    /// Each capability's name, as a job carries them.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.iter().map(|cap| cap.name().to_string()).collect()
    }

    /// Those of these that `allowed` does not hold.
    #[must_use]
    pub fn outside(&self, allowed: &Self) -> Self {
        Self(self.0.difference(&allowed.0).copied().collect())
    }
}

impl FromIterator<Capability> for Capabilities {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl FromStr for Capabilities {
    type Err = UnknownCapability;

    /// Names separated by commas or whitespace, `CAP_NET_ADMIN, net_raw`;
    /// empty is none.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_names(
            s.split(|c: char| c == ',' || c.is_whitespace())
                .filter(|name| !name.is_empty()),
        )
    }
}

/// Comma-separated names, `CAP_NET_ADMIN,CAP_NET_RAW`; empty for none.
impl fmt::Display for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.names().join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL` and `NAMES` are both in the kernel's numbering, so a capability's
    /// position in either is its number.
    #[test]
    fn the_table_is_in_the_kernels_numbering() {
        for (number, cap) in Capability::ALL.into_iter().enumerate() {
            assert_eq!(cap as usize, number, "{cap:?}");
        }
        assert_eq!(Capability::NetAdmin.name(), "CAP_NET_ADMIN");
        assert_eq!(Capability::NetAdmin as usize, 12);
        assert_eq!(Capability::SysAdmin as usize, 21);
        assert_eq!(Capability::CheckpointRestore as usize, 40);
    }

    #[test]
    fn a_name_reads_in_any_case_with_or_without_its_prefix() {
        for spelling in [
            "CAP_NET_ADMIN",
            "cap_net_admin",
            "NET_ADMIN",
            "net_admin",
            " Net_Admin ",
        ] {
            assert_eq!(spelling.parse(), Ok(Capability::NetAdmin), "{spelling}");
        }
        for name in Capability::ALL.map(Capability::name) {
            assert_eq!(name.parse::<Capability>().map(Capability::name), Ok(name));
        }
    }

    #[test]
    fn an_unknown_name_is_refused() {
        for name in ["CAP_NET", "NETADMIN", "CAP_", "", "CAP_CAP_NET_ADMIN"] {
            assert!(name.parse::<Capability>().is_err(), "{name:?}");
        }
        let err = "net_admn".parse::<Capability>().unwrap_err();
        assert!(err.to_string().contains("\"net_admn\""), "{err}");
    }

    /// A list is normalised: each capability once, in the kernel's order,
    /// under its full name.
    #[test]
    fn a_list_is_normalised() {
        let caps: Capabilities = "net_raw, CAP_NET_ADMIN  net_raw\n".parse().unwrap();
        assert_eq!(caps.to_string(), "CAP_NET_ADMIN,CAP_NET_RAW");
        assert_eq!(caps.names(), ["CAP_NET_ADMIN", "CAP_NET_RAW"]);
        assert_eq!("".parse::<Capabilities>(), Ok(Capabilities::none()));
        assert_eq!(" , ".parse::<Capabilities>(), Ok(Capabilities::none()));
        assert!("net_admin,bogus".parse::<Capabilities>().is_err());
    }

    #[test]
    fn outside_is_what_the_allowance_lacks() {
        let wanted: Capabilities = "net_admin,net_raw".parse().unwrap();
        let allowed: Capabilities = "net_raw,sys_time".parse().unwrap();
        assert_eq!(wanted.outside(&allowed).to_string(), "CAP_NET_ADMIN");
        assert!(wanted.outside(&wanted).is_empty());
    }

    /// The ones a sandbox exists to withhold. `CAP_NET_ADMIN`, the usual
    /// reason to ask, is not one of them: it is the operator's call.
    #[test]
    fn the_sandbox_breaking_capabilities() {
        let breaking: Vec<_> = Capability::ALL
            .into_iter()
            .filter(|cap| cap.breaks_confinement())
            .map(Capability::name)
            .collect();
        assert_eq!(
            breaking,
            [
                "CAP_DAC_READ_SEARCH",
                "CAP_SYS_MODULE",
                "CAP_SYS_RAWIO",
                "CAP_SYS_PTRACE",
                "CAP_SYS_ADMIN",
                "CAP_SYS_BOOT",
                "CAP_MAC_OVERRIDE",
                "CAP_MAC_ADMIN",
                "CAP_PERFMON",
                "CAP_BPF",
            ]
        );
        assert!(!Capability::NetAdmin.breaks_confinement());
    }
}
