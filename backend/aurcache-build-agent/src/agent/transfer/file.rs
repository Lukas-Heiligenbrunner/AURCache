//! Reading and writing files in fixed-size pieces, and opening only what a
//! build may legitimately have left.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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

/// Open `path` for reading if it is a regular file.
///
/// The open itself neither follows a symbolic link nor waits on a pipe nor
/// takes a terminal, so a name the build made into any of those cannot hold
/// the agent up or point it elsewhere.
pub fn open_regular(path: &Path) -> io::Result<Opened> {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        // ELOOP: the last component is a symbolic link.
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(Opened::Other),
        Err(error) => return Err(error),
    };
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
    use super::{CHUNK, Opened, open_regular, pump};
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

    #[test]
    fn open_regular_takes_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        symlink("f", dir.path().join("l")).unwrap();
        assert!(matches!(
            open_regular(&dir.path().join("f")).unwrap(),
            Opened::Regular(_)
        ));
        assert!(matches!(
            open_regular(&dir.path().join("d")).unwrap(),
            Opened::Other
        ));
        assert!(matches!(
            open_regular(&dir.path().join("l")).unwrap(),
            Opened::Other
        ));
        assert!(open_regular(&dir.path().join("missing")).is_err());
    }
}
