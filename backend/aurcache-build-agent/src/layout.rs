//! Where things are, inside a build container: the names and paths both the
//! agent and the workers that render its container rely on.

use std::path::{Path, PathBuf};

pub const BIN_NAME: &str = "aurcache-build-agent";
/// The capabilities the build container's root has, and no others.
pub const CAPABILITIES: [&str; 7] = [
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "SETUID",
    "SETGID",
    "KILL",
];
pub const BUILD_UID: u32 = 1001;
pub const BUILD_GID: u32 = 1001;
pub const INPUT_SPEC: &str = "spec.json";
pub const INPUT_SOURCE: &str = "source.tar.gz";
/// The variable makepkg's environment carries the helper's socket in.
pub const PACMAN_SOCKET_VAR: &str = "AURCACHE_PACMAN_SOCKET";

/// The directory a worker gives the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work(PathBuf);

impl Work {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self(root.into())
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.0
    }

    /// `<root>/in`: what the worker hands in.
    #[must_use]
    pub fn input(&self) -> PathBuf {
        self.0.join("in")
    }

    /// `<root>/in/spec.json`.
    #[must_use]
    pub fn spec(&self) -> PathBuf {
        self.input().join(INPUT_SPEC)
    }

    /// `<root>/in/source.tar.gz`.
    #[must_use]
    pub fn source(&self) -> PathBuf {
        self.input().join(INPUT_SOURCE)
    }

    /// `<root>/in/ready`: present once both inputs are complete.
    #[must_use]
    pub fn ready(&self) -> PathBuf {
        self.input().join("ready")
    }

    /// `<root>/pkg`: makepkg's `PKGDEST`, what the worker collects.
    #[must_use]
    pub fn pkg(&self) -> PathBuf {
        self.0.join("pkg")
    }

    /// `<root>/log/build.log`.
    #[must_use]
    pub fn log(&self) -> PathBuf {
        self.0.join("log").join("build.log")
    }

    /// `<root>/out`: root's, until the build is over.
    #[must_use]
    pub fn out(&self) -> PathBuf {
        self.0.join("out")
    }

    /// `<root>/out/report.json`.
    #[must_use]
    pub fn report(&self) -> PathBuf {
        self.out().join("report.json")
    }
}

/// The build's own trees, which no worker reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self(root.into())
    }

    /// `<root>/src/<pkgbase>`: the extracted source, makepkg's start directory.
    #[must_use]
    pub fn startdir(&self, pkgbase: &str) -> PathBuf {
        self.src().join(pkgbase)
    }

    /// `<root>/src`.
    #[must_use]
    pub fn src(&self) -> PathBuf {
        self.0.join("src")
    }

    /// `<root>/build`: makepkg's `BUILDDIR`.
    #[must_use]
    pub fn build(&self) -> PathBuf {
        self.0.join("build")
    }

    /// `<root>/srcdest`: makepkg's `SRCDEST`.
    #[must_use]
    pub fn srcdest(&self) -> PathBuf {
        self.0.join("srcdest")
    }

    /// `<root>/home`: the build user's home and keyring.
    #[must_use]
    pub fn home(&self) -> PathBuf {
        self.0.join("home")
    }

    /// `<root>/makepkg.conf`.
    #[must_use]
    pub fn makepkg_conf(&self) -> PathBuf {
        self.0.join("makepkg.conf")
    }

    /// `<root>/run/pacman.sock`: the pacman helper's socket.
    #[must_use]
    pub fn pacman_socket(&self) -> PathBuf {
        self.0.join("run").join("pacman.sock")
    }

    /// `<root>/run/ssh-agent.sock`.
    #[must_use]
    pub fn ssh_socket(&self) -> PathBuf {
        self.0.join("run").join("ssh-agent.sock")
    }
}

#[cfg(test)]
mod tests {
    use super::{Scratch, Work};
    use std::path::Path;

    #[test]
    fn work_paths() {
        let work = Work::new("/work");
        assert_eq!(work.root(), Path::new("/work"));
        assert_eq!(work.input(), Path::new("/work/in"));
        assert_eq!(work.spec(), Path::new("/work/in/spec.json"));
        assert_eq!(work.source(), Path::new("/work/in/source.tar.gz"));
        assert_eq!(work.ready(), Path::new("/work/in/ready"));
        assert_eq!(work.pkg(), Path::new("/work/pkg"));
        assert_eq!(work.log(), Path::new("/work/log/build.log"));
        assert_eq!(work.out(), Path::new("/work/out"));
        assert_eq!(work.report(), Path::new("/work/out/report.json"));
    }

    #[test]
    fn scratch_paths() {
        let scratch = Scratch::new("/s");
        assert_eq!(scratch.startdir("hello"), Path::new("/s/src/hello"));
        assert_eq!(scratch.src(), Path::new("/s/src"));
        assert_eq!(scratch.build(), Path::new("/s/build"));
        assert_eq!(scratch.srcdest(), Path::new("/s/srcdest"));
        assert_eq!(scratch.home(), Path::new("/s/home"));
        assert_eq!(scratch.makepkg_conf(), Path::new("/s/makepkg.conf"));
        assert_eq!(scratch.pacman_socket(), Path::new("/s/run/pacman.sock"));
        assert_eq!(scratch.ssh_socket(), Path::new("/s/run/ssh-agent.sock"));
    }
}
