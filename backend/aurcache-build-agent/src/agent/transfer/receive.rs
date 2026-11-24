//! Take the job in from stdin: the spec, then the source archive.

use aurcache_build_agent::handin::read_len;
use aurcache_build_agent::layout::Work;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Read the job into `<work>/in/`, and mark it ready with this process's id.
///
/// Neither input has its name until both arrived whole, and the job is
/// marked ready only then. A job already handed in is refused: a build may
/// have started on it.
pub fn run(work: &Work, from: &mut impl BufRead) -> io::Result<()> {
    fs::create_dir_all(work.input())?;
    if fs::symlink_metadata(work.ready()).is_ok() {
        return Err(io::Error::other("the job was already handed in"));
    }
    let inputs = [work.spec(), work.source()];
    let taken = take(&inputs, from).and_then(|()| {
        for input in &inputs {
            fs::rename(part(input), input)?;
        }
        let ready = part(&work.ready());
        create(&ready)?.write_all(format!("{}\n", std::process::id()).as_bytes())?;
        fs::rename(&ready, work.ready())
    });
    if taken.is_err() {
        for input in &inputs {
            let _ = fs::remove_file(part(input));
        }
    }
    taken
}

/// Write each input to its part file, as long as its length says.
fn take(inputs: &[PathBuf], from: &mut impl BufRead) -> io::Result<()> {
    for input in inputs {
        let len = read_len(from)?;
        let mut file = create(&part(input))?;
        let written = io::copy(&mut Read::take(&mut *from, len), &mut file)?;
        if written != len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("the job ended {} bytes into {}", written, input.display()),
            ));
        }
    }
    Ok(())
}

/// Where an input is written before it has its name.
fn part(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.part"))
}

/// A new file, mode 0644, that must not exist (and so is not a link).
fn create(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::run;
    use aurcache_build_agent::handin::prefix;
    use aurcache_build_agent::layout::Work;

    fn job(spec: &[u8], source: &[u8]) -> Vec<u8> {
        [
            prefix(spec.len()).as_bytes(),
            spec,
            prefix(source.len()).as_bytes(),
            source,
        ]
        .concat()
    }

    fn names(work: &Work) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(work.input())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Both inputs land, the job is marked ready, and nothing after the
    /// source is read: the stream need not end.
    #[test]
    fn both_inputs_land_and_the_job_is_marked_ready() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        let mut stream = job(b"{}", b"src");
        stream.extend_from_slice(b"left unread");
        let mut reader = stream.as_slice();
        run(&work, &mut reader).unwrap();
        assert_eq!(reader, b"left unread");
        assert_eq!(std::fs::read(work.spec()).unwrap(), b"{}");
        assert_eq!(std::fs::read(work.source()).unwrap(), b"src");
        assert_eq!(names(&work), ["ready", "source.tar.gz", "spec.json"]);
        assert!(run(&work, &mut job(b"{}", b"again").as_slice()).is_err());
        assert_eq!(std::fs::read(work.source()).unwrap(), b"src");
    }

    #[test]
    fn a_job_cut_short_or_malformed_leaves_nothing() {
        let whole = job(b"{}", b"src");
        for bad in [&whole[..whole.len() - 1], b"2\n{}", b"x\n{}3\nsrc"] {
            let dir = tempfile::tempdir().unwrap();
            let work = Work::new(dir.path());
            assert!(run(&work, &mut &bad[..]).is_err(), "{bad:?}");
            assert!(names(&work).is_empty(), "{bad:?}");
        }
    }
}
