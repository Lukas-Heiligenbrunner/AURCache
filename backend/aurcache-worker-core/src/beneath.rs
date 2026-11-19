//! Reading a directory something else could write to: one name at a time,
//! from the handle of the directory it is in, following nothing.
//!
//! A build's directory is the worker's, but what is in it is the build's.
//! Root in a build container (a dependency's install scriptlet) can put a
//! link, a pipe or a file of any size in place of anything the worker reads
//! back. So every name here is opened from its parent's handle with
//! `O_NOFOLLOW` and `O_NONBLOCK`, and checked for what it is once it is open:
//! a path never leads out of the directory, an open never waits on a pipe,
//! and a read stops at a limit.

use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
use rustix::io::Errno;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// A directory opened as itself, never as what a link at its place points
/// at.
#[derive(Debug)]
pub struct Dir {
    fd: OwnedFd,
    path: PathBuf,
}

impl Dir {
    /// Open `path`, which must be a directory and not a link to one. Only
    /// its last name is held to that: the ones above it are the caller's.
    pub fn open(path: &Path) -> io::Result<Self> {
        // The last name is opened from its parent, so that a trailing slash,
        // which would have the kernel follow a link, never reaches it.
        let fd = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
                let above = rustix::fs::open(parent, flags, Mode::empty()).map_err(|errno| {
                    let error = io::Error::from(errno);
                    io::Error::new(error.kind(), format!("{}: {error}", parent.display()))
                })?;
                open_as(&above, Path::new(name), path, FileType::Directory)?
            }
            _ => open_as(rustix::fs::CWD, path, path, FileType::Directory)?,
        };
        Ok(Self {
            fd,
            path: path.to_path_buf(),
        })
    }

    /// The directory `name`, directly in this one.
    pub fn dir(&self, name: impl AsRef<OsStr>) -> io::Result<Self> {
        let (name, path) = self.entry(name.as_ref())?;
        let fd = open_as(&self.fd, name, &path, FileType::Directory)?;
        Ok(Self { fd, path })
    }

    /// The regular file `name`, directly in this one, open for reading.
    pub fn file(&self, name: impl AsRef<OsStr>) -> io::Result<File> {
        let (name, path) = self.entry(name.as_ref())?;
        open_as(&self.fd, name, &path, FileType::RegularFile).map(File::from)
    }

    /// The whole of the regular file `name`, directly in this one; refused
    /// when it holds more than `limit` bytes.
    pub fn read(&self, name: impl AsRef<OsStr>, limit: u64) -> io::Result<Vec<u8>> {
        let name = name.as_ref();
        let mut bytes = Vec::new();
        self.file(name)?
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is larger than {limit} bytes",
                    self.path.join(name).display()
                ),
            ));
        }
        Ok(bytes)
    }

    /// The names directly in this directory, `.` and `..` left out, in no
    /// particular order.
    pub fn names(&self) -> io::Result<Vec<OsString>> {
        let entries = rustix::fs::Dir::read_from(&self.fd).map_err(|e| self.error(e))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| self.error(e))?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name != "." && name != ".." {
                names.push(name.to_os_string());
            }
        }
        Ok(names)
    }

    /// The path this directory was opened as, for messages.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `name` as one name in this directory, and the path it stands for.
    /// Without a trailing slash, which would have the kernel follow a link.
    fn entry<'a>(&self, name: &'a OsStr) -> io::Result<(&'a Path, PathBuf)> {
        let path = Path::new(name);
        let mut components = path.components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(one)), None) => {
                let one = Path::new(one);
                Ok((one, self.path.join(one)))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path:?} is not one name in {}", self.path.display()),
            )),
        }
    }

    fn error(&self, errno: Errno) -> io::Error {
        let error = io::Error::from(errno);
        io::Error::new(error.kind(), format!("{}: {error}", self.path.display()))
    }
}

/// Open `name` in `dir` without following it or waiting on it, and refuse
/// it unless it is a `want`. `shown` is how it is named in an error.
fn open_as(dir: impl AsFd, name: &Path, shown: &Path, want: FileType) -> io::Result<OwnedFd> {
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let fd = openat(dir, name, flags, Mode::empty()).map_err(|errno| {
        let error = io::Error::from(errno);
        match errno {
            Errno::LOOP => refused(shown, "a link, which is not followed"),
            _ => io::Error::new(error.kind(), format!("{}: {error}", shown.display())),
        }
    })?;
    let found = FileType::from_raw_mode(fstat(&fd)?.st_mode);
    if found == want {
        Ok(fd)
    } else {
        Err(refused(shown, kind(found)))
    }
}

fn refused(path: &Path, what: &str) -> io::Error {
    let expected = format!("{} is {what}", path.display());
    io::Error::new(io::ErrorKind::InvalidData, expected)
}

/// What a file is, as a message says it.
fn kind(found: FileType) -> &'static str {
    match found {
        FileType::RegularFile => "a regular file",
        FileType::Directory => "a directory",
        FileType::Symlink => "a link, which is not followed",
        FileType::Fifo => "a pipe",
        FileType::Socket => "a socket",
        FileType::CharacterDevice | FileType::BlockDevice => "a device",
        FileType::Unknown => "of an unknown kind",
    }
}

#[cfg(test)]
mod tests {
    use super::Dir;
    use std::ffi::CString;
    use std::fs;
    use std::io::ErrorKind;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::Duration;

    fn fifo(path: &Path) {
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid, NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o644) }, 0);
    }

    /// Run `f` on a thread, failing rather than hanging when it does not
    /// return: what is under test is that nothing here waits.
    fn promptly<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || send.send(f()));
        receive
            .recv_timeout(Duration::from_secs(10))
            .expect("returned within ten seconds")
    }

    #[test]
    fn a_file_in_a_directory_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("out")).unwrap();
        fs::write(tmp.path().join("out/report.json"), b"{}").unwrap();
        let out = Dir::open(tmp.path()).unwrap().dir("out").unwrap();
        assert_eq!(out.read("report.json", 2).unwrap(), b"{}");
        assert_eq!(out.path(), tmp.path().join("out"));
        assert_eq!(out.names().unwrap(), ["report.json"]);
    }

    #[test]
    fn what_is_not_there_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Dir::open(tmp.path()).unwrap();
        assert_eq!(dir.dir("out").unwrap_err().kind(), ErrorKind::NotFound);
        assert_eq!(dir.read("x", 1).unwrap_err().kind(), ErrorKind::NotFound);
        let gone = tmp.path().join("gone");
        assert_eq!(Dir::open(&gone).unwrap_err().kind(), ErrorKind::NotFound);
    }

    /// A link is refused wherever it is: as the directory opened, as a
    /// directory in it, as the file read. What it points at may be a
    /// directory of the worker's own, or a device that never ends.
    #[test]
    fn a_link_is_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        fs::write(elsewhere.path().join("secret"), b"the worker's").unwrap();
        symlink(elsewhere.path(), tmp.path().join("pkg")).unwrap();
        symlink(elsewhere.path().join("secret"), tmp.path().join("file")).unwrap();
        symlink("/dev/zero", tmp.path().join("zero")).unwrap();

        let dir = Dir::open(tmp.path()).unwrap();
        for refused in [
            dir.dir("pkg").unwrap_err(),
            dir.dir("pkg/").unwrap_err(),
            Dir::open(&tmp.path().join("pkg/")).unwrap_err(),
            dir.read("file", 1 << 20).unwrap_err(),
            promptly(move || dir.read("zero", 1 << 20).unwrap_err()),
            Dir::open(&tmp.path().join("pkg")).unwrap_err(),
        ] {
            assert_eq!(refused.kind(), ErrorKind::InvalidData, "{refused}");
            assert!(refused.to_string().contains("a link"), "{refused}");
        }
    }

    /// A pipe is refused at once, where reading it would wait for a writer
    /// that never comes.
    #[test]
    fn a_pipe_is_refused_without_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        fifo(&tmp.path().join("report.json"));
        let path = tmp.path().to_path_buf();
        let refused = promptly(move || {
            Dir::open(&path)
                .unwrap()
                .read("report.json", 1 << 20)
                .unwrap_err()
        });
        assert!(refused.to_string().contains("a pipe"), "{refused}");
    }

    #[test]
    fn a_file_where_a_directory_belongs_is_refused_and_so_is_the_reverse() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("pkg"), b"").unwrap();
        fs::create_dir(tmp.path().join("report.json")).unwrap();
        let dir = Dir::open(tmp.path()).unwrap();
        let refused = dir.dir("pkg").unwrap_err();
        assert!(refused.to_string().contains("a regular file"), "{refused}");
        let refused = dir.read("report.json", 1).unwrap_err();
        assert!(refused.to_string().contains("a directory"), "{refused}");
    }

    #[test]
    fn a_file_past_the_limit_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("big"), vec![b'x'; 11]).unwrap();
        let dir = Dir::open(tmp.path()).unwrap();
        assert_eq!(dir.read("big", 11).unwrap().len(), 11);
        let refused = dir.read("big", 10).unwrap_err();
        assert!(refused.to_string().contains("larger than 10"), "{refused}");
    }

    /// Only one name at a time: a path that would climb out, or go through
    /// a directory without opening it, is not one.
    #[test]
    fn a_name_is_one_name() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("out")).unwrap();
        let dir = Dir::open(tmp.path()).unwrap();
        for name in ["..", ".", "", "out/x", "/etc", "../etc"] {
            let refused = dir.dir(name).unwrap_err();
            assert_eq!(refused.kind(), ErrorKind::InvalidInput, "{name:?}");
        }
    }
}
