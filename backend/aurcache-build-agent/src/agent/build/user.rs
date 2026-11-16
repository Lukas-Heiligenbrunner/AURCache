//! The build user: its account, and the one way a command is run as it.
//!
//! Everything after the root step runs through [`BuildUser::command`], so no
//! step can forget the drop: the build user's uid and gid with no other
//! group, `no_new_privs`, no capability in any set a process can clear, no
//! controlling terminal, and none of root's environment.

use aurcache_build_agent::layout::{BUILD_GID, BUILD_UID};
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The name the account gets where the image has none for its uid.
const NAME: &str = "aurbuild";

/// What a build finds programs through before its login shell adds to it.
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/bin";

const PASSWD: &str = "/etc/passwd";
const GROUP: &str = "/etc/group";
const SHADOW: &str = "/etc/shadow";

/// The account builds run as.
pub struct BuildUser {
    /// The name the image's user database gives [`BUILD_UID`].
    pub name: String,
    pub home: PathBuf,
}

impl BuildUser {
    /// Make sure the build's uid and gid have names, and return the account.
    ///
    /// The lines are written directly: `useradd` would pick ids, make a home
    /// and read a configuration that is the image's. An image that already
    /// has the uid keeps its own line.
    pub fn ensure(home: &Path) -> io::Result<Self> {
        let passwd = std::fs::read_to_string(PASSWD)?;
        let name = if let Some(name) = named(&passwd, BUILD_UID) {
            name
        } else {
            let name = unused(&passwd, NAME);
            let line = format!(
                "{name}:x:{BUILD_UID}:{BUILD_GID}:AURCache build:{}:/bin/bash",
                home.display()
            );
            append(PASSWD, &passwd, &line)?;
            if let Ok(shadow) = std::fs::read_to_string(SHADOW)
                && !has_name(&shadow, &name)
            {
                // A locked password and no ageing.
                append(SHADOW, &shadow, &format!("{name}:!*:::::::"))?;
            }
            name
        };
        let group = std::fs::read_to_string(GROUP)?;
        if named(&group, BUILD_GID).is_none() {
            let line = format!("{}:x:{BUILD_GID}:", unused(&group, &name));
            append(GROUP, &group, &line)?;
        }
        Ok(Self {
            name,
            home: home.to_path_buf(),
        })
    }

    /// This program, as the build user, with none of the agent's own
    /// streams: whoever starts it says where its output goes.
    ///
    /// The standard library sets the gid, clears the supplementary groups
    /// and sets the uid, in that order, before it changes directory; `seal`
    /// then runs in the child, just before the program replaces it.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("USER", &self.name)
            .env("LOGNAME", &self.name)
            .env("SHELL", "/bin/bash")
            .env("PATH", PATH)
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .uid(BUILD_UID)
            .gid(BUILD_GID);
        // SAFETY: `seal` makes system calls and nothing else: it allocates
        // nothing and takes no lock, as a child of a threaded process must.
        unsafe { command.pre_exec(seal) };
        command
    }
}

/// The kernel's capability sets of one process, as `capget` and `capset`
/// exchange them.
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// `_LINUX_CAPABILITY_VERSION_3`: 64 bits per set, as two words.
const CAP_VERSION: u32 = 0x2008_0522;

const NO_CAPS: [CapData; 2] = [CapData {
    effective: 0,
    permitted: 0,
    inheritable: 0,
}; 2];

/// Every number a capability can have: both words of a set.
const CAP_NUMBERS: libc::c_ulong = 64;

#[allow(clippy::cast_sign_loss)]
const AMBIENT_CLEAR_ALL: libc::c_ulong = libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong;
#[allow(clippy::cast_sign_loss)]
const AMBIENT_IS_SET: libc::c_ulong = libc::PR_CAP_AMBIENT_IS_SET as libc::c_ulong;

/// `prctl` with every argument as wide as the kernel reads it.
///
/// # Safety
///
/// As for `prctl(2)` with the given option.
unsafe fn prctl(option: libc::c_int, first: libc::c_ulong, second: libc::c_ulong) -> libc::c_int {
    let none: libc::c_ulong = 0;
    // SAFETY: the caller's.
    unsafe { libc::prctl(option, first, second, none, none) }
}

/// Finish the drop, in the child, and refuse to go on unless it is complete.
///
/// Leaving the session takes the controlling terminal away. `no_new_privs`
/// makes every later `execve` ignore set-user-id bits and file capabilities,
/// which is what stops `sudo`. Becoming a non-root user already emptied the
/// permitted and effective sets; the ambient and inheritable ones are emptied
/// here. Then everything is read back: a drop that only partly happened ends
/// the child instead of running a build with what was left.
fn seal() -> io::Result<()> {
    // SAFETY: plain system calls on this process, with valid pointers to
    // locals where one is passed.
    unsafe {
        if libc::setsid() == -1 {
            return Err(io::Error::last_os_error());
        }
        if prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        // Not every kernel has an ambient set to clear; what matters is read
        // back below.
        prctl(libc::PR_CAP_AMBIENT, AMBIENT_CLEAR_ALL, 0);
        let header = CapHeader {
            version: CAP_VERSION,
            pid: 0,
        };
        if libc::syscall(libc::SYS_capset, &raw const header, NO_CAPS.as_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if dropped() {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(libc::EPERM))
    }
}

/// Whether this process is the build user and nothing more.
fn dropped() -> bool {
    let (mut ruid, mut euid, mut suid) = (0, 0, 0);
    let (mut rgid, mut egid, mut sgid) = (0, 0, 0);
    let header = CapHeader {
        version: CAP_VERSION,
        pid: 0,
    };
    let mut caps = [CapData {
        effective: u32::MAX,
        permitted: u32::MAX,
        inheritable: u32::MAX,
    }; 2];
    // SAFETY: plain system calls reading this process's own state into
    // locals of the right size.
    unsafe {
        libc::getresuid(&raw mut ruid, &raw mut euid, &raw mut suid) == 0
            && [ruid, euid, suid] == [BUILD_UID; 3]
            && libc::getresgid(&raw mut rgid, &raw mut egid, &raw mut sgid) == 0
            && [rgid, egid, sgid] == [BUILD_GID; 3]
            && libc::getgroups(0, std::ptr::null_mut()) == 0
            && prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0) == 1
            && (0..CAP_NUMBERS).all(|cap| prctl(libc::PR_CAP_AMBIENT, AMBIENT_IS_SET, cap) <= 0)
            && libc::syscall(libc::SYS_capget, &raw const header, caps.as_mut_ptr()) == 0
            && caps
                .iter()
                .all(|set| set.effective | set.permitted | set.inheritable == 0)
    }
}

/// The name a user or group database gives an id.
fn named(database: &str, id: u32) -> Option<String> {
    database.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        (fields.nth(1)?.parse() == Ok(id) && !name.is_empty()).then(|| name.to_string())
    })
}

/// Whether a user, group or shadow database has an entry of this name.
fn has_name(database: &str, name: &str) -> bool {
    database
        .lines()
        .any(|line| line.split(':').next() == Some(name))
}

/// `base`, or `base` with the first number that makes it a name the database
/// does not have.
fn unused(database: &str, base: &str) -> String {
    if !has_name(database, base) {
        return base.to_string();
    }
    (1u32..)
        .map(|n| format!("{base}{n}"))
        .find(|name| !has_name(database, name))
        .unwrap_or_else(|| base.to_string())
}

/// Add one line to a database, after a newline if its last line has none.
fn append(path: &str, current: &str, line: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    let gap = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    file.write_all(format!("{gap}{line}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{has_name, named, unused};

    const PASSWD: &str = "root:x:0:0::/root:/usr/bin/bash\n\
                          bin:x:1:1::/:/usr/bin/nologin\n\
                          ab:x:968:968::/var/ab:/usr/bin/nologin\n\
                          odd line without fields\n\
                          :x:77:77::/:/usr/bin/nologin\n";

    #[test]
    fn an_id_is_found_by_its_own_field() {
        assert_eq!(named(PASSWD, 968).as_deref(), Some("ab"));
        assert_eq!(named(PASSWD, 0).as_deref(), Some("root"));
        // 1 is `bin`'s uid, and also other lines' gid.
        assert_eq!(named(PASSWD, 1).as_deref(), Some("bin"));
        assert_eq!(named(PASSWD, 1001), None);
        // A line with no name is not an account.
        assert_eq!(named(PASSWD, 77), None);
        assert_eq!(
            named("ab:x:968:\nwheel:x:998:ab\n", 998).as_deref(),
            Some("wheel")
        );
    }

    #[test]
    fn a_taken_name_gets_a_number() {
        assert_eq!(unused(PASSWD, "aurbuild"), "aurbuild");
        assert_eq!(unused(PASSWD, "ab"), "ab1");
        assert_eq!(unused("ab:x:1:\nab1:x:2:\n", "ab"), "ab2");
        assert!(has_name(PASSWD, "bin"));
        assert!(!has_name(PASSWD, "b"));
    }
}
