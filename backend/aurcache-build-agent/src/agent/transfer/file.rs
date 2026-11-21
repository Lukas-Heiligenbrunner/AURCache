//! Reading and writing files in fixed-size pieces, and opening only what a
//! build may legitimately have left.

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// How much of a file is in memory at once.
pub const CHUNK: usize = 64 * 1024;

/// What a path under a directory the build wrote to turned out to be.
pub enum Opened {
    Regular(File),
    /// A directory, a device, a pipe, a socket: anything but a file, or a
    /// symbolic link, which is never followed.
    Other,
}

/// A directory directly in the work directory, opened as itself.
pub struct Beneath(OwnedFd);

/// What was at a directory's name in the work directory.
pub enum OpenedDir {
    Dir(Beneath),
    /// A symbolic link, never followed, or anything but a directory.
    Other,
}

/// Open the directory `name` in `root`, the work directory, without
/// following a link at its place.
///
/// `root` is a volume's mount point, which nothing in a container can
/// replace; what is in it, root in the build container could have made
/// anything, a link to elsewhere in this container included.
pub fn open_dir_in(root: &Path, name: &str) -> io::Result<OpenedDir> {
    let root = rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat(&root, name, flags, Mode::empty()) {
        Ok(dir) => Ok(OpenedDir::Dir(Beneath(dir))),
        // ELOOP: a symbolic link; ENOTDIR: anything else but a directory.
        Err(Errno::LOOP | Errno::NOTDIR) => Ok(OpenedDir::Other),
        Err(errno) => Err(errno.into()),
    }
}

impl Beneath {
    /// The regular file `name` in this directory, if it is one.
    ///
    /// The open itself neither follows a symbolic link nor waits on a pipe
    /// nor takes a terminal, so a name the build made into any of those
    /// cannot hold the agent up or point it elsewhere.
    pub fn regular(&self, name: &str) -> io::Result<Opened> {
        let flags =
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
        let file = match rustix::fs::openat(&self.0, name, flags, Mode::empty()) {
            Ok(fd) => File::from(fd),
            Err(Errno::LOOP) => return Ok(Opened::Other),
            Err(errno) => return Err(errno.into()),
        };
        regular(file)
    }

    /// The names in this directory that are UTF-8, `.` and `..` left out.
    pub fn names(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(&self.0)? {
            let name = entry?.file_name().to_bytes().to_vec();
            if let Ok(name) = String::from_utf8(name)
                && name != "."
                && name != ".."
            {
                names.push(name);
            }
        }
        Ok(names)
    }
}

/// `file` if it is a regular file.
fn regular(file: File) -> io::Result<Opened> {
    if file.metadata()?.mode() & libc::S_IFMT == libc::S_IFREG {
        Ok(Opened::Regular(file))
    } else {
        Ok(Opened::Other)
    }
}

/// Copy `from` to `to` through one buffer of [`CHUNK`] bytes, and say how
/// many bytes that was. Stops at the first write error.
pub fn pump(from: &mut impl Read, to: &mut impl Write) -> io::Result<u64> {
    let mut buffer = vec![0_u8; CHUNK];
    let mut total = 0_u64;
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        to.write_all(&buffer[..read])?;
        total += read as u64;
    }
}

/// The process's standard output, unbuffered and not line-scanned, for bytes
/// that are not text. It does not close the descriptor when dropped.
pub fn stdout() -> ManuallyDrop<File> {
    // SAFETY: descriptor 1 is open for the life of the process, and the
    // `ManuallyDrop` keeps this handle from closing it.
    ManuallyDrop::new(unsafe { File::from_raw_fd(libc::STDOUT_FILENO) })
}

#[cfg(test)]
mod tests {
    use super::{CHUNK, Opened, OpenedDir, open_dir_in, pump};
    use std::io::Cursor;
    use std::os::unix::fs::symlink;

    #[test]
    fn pump_copies_more_than_one_chunk_exactly() {
        let data: Vec<u8> = (0..CHUNK * 2 + 17).map(|i| (i % 251) as u8).collect();
        let mut out = Vec::new();
        let count = pump(&mut Cursor::new(&data), &mut out).unwrap();
        assert_eq!(count, data.len() as u64);
        assert_eq!(out, data);
    }

    /// A directory that is a link, or not a directory, is not opened; nor
    /// is a link or a pipe in one, which is not waited on.
    #[test]
    fn a_directory_in_the_work_directory_is_opened_as_itself() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("f"), b"elsewhere").unwrap();
        std::fs::create_dir(root.path().join("pkg")).unwrap();
        std::fs::write(root.path().join("pkg/f"), b"x").unwrap();
        symlink("f", root.path().join("pkg/l")).unwrap();
        let fifo = std::ffi::CString::new(root.path().join("pkg/p").to_str().unwrap()).unwrap();
        // SAFETY: a valid, NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        symlink(elsewhere.path(), root.path().join("linked")).unwrap();
        std::fs::write(root.path().join("file"), b"").unwrap();

        let OpenedDir::Dir(pkg) = open_dir_in(root.path(), "pkg").unwrap() else {
            panic!("pkg/ is a directory");
        };
        let mut names = pkg.names().unwrap();
        names.sort();
        assert_eq!(names, ["f", "l", "p"]);
        assert!(matches!(pkg.regular("f").unwrap(), Opened::Regular(_)));
        assert!(matches!(pkg.regular("l").unwrap(), Opened::Other));
        assert!(matches!(pkg.regular("p").unwrap(), Opened::Other));
        assert!(pkg.regular("missing").is_err());
        for other in ["linked", "file"] {
            assert!(matches!(
                open_dir_in(root.path(), other).unwrap(),
                OpenedDir::Other
            ));
        }
        let missing = open_dir_in(root.path(), "missing").err().unwrap();
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
    }
}
