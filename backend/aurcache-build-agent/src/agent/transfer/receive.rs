//! Take the job in: a tar of the spec and the source archive, from stdin.

use super::file::pump;
use anyhow::{Context, Result, bail, ensure};
use aurcache_build_agent::layout::{INPUT_SOURCE, INPUT_SPEC, Work};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use tar::{Archive, EntryType};

/// The two files a job consists of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Input {
    Spec,
    Source,
}

impl Input {
    const ALL: [Self; 2] = [Self::Spec, Self::Source];

    /// The input an entry name stands for. Only the exact names: not a path.
    fn named(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|input| input.name().as_bytes() == name)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Spec => INPUT_SPEC,
            Self::Source => INPUT_SOURCE,
        }
    }

    fn part(self, work: &Work) -> PathBuf {
        work.input().join(format!(".{}.part", self.name()))
    }

    fn path(self, work: &Work) -> PathBuf {
        work.input().join(self.name())
    }
}

/// Read the job from `reader` into `<work>/in/`, and mark it ready.
///
/// The tar must hold each input once as a regular file and nothing else. It
/// is read to its end-of-archive marker and no further, so the sender need
/// not close its end. Entry names are matched, never joined onto a path: the
/// only files written are the two fixed ones. Nothing is visible under its
/// real name until both arrived whole, and nothing is marked ready unless
/// they did.
pub fn run(work: &Work, reader: impl Read) -> Result<()> {
    fs::create_dir_all(work.input())
        .with_context(|| format!("cannot create {}", work.input().display()))?;
    ensure!(
        fs::symlink_metadata(work.ready()).is_err(),
        "the job was already handed in"
    );
    let result = take(work, reader);
    if result.is_err() {
        for input in Input::ALL {
            let _ = fs::remove_file(input.part(work));
        }
    }
    result
}

fn take(work: &Work, reader: impl Read) -> Result<()> {
    let mut seen = Vec::new();
    for entry in Archive::new(reader)
        .entries()
        .context("not a tar archive")?
    {
        let mut entry = entry.context("cannot read a tar entry")?;
        let name = entry.path_bytes().into_owned();
        let shown = String::from_utf8_lossy(&name).into_owned();
        ensure!(
            entry.header().entry_type() == EntryType::Regular,
            "entry {shown:?} is not a regular file"
        );
        let Some(input) = Input::named(&name) else {
            bail!("entry {shown:?} is not part of a job");
        };
        ensure!(!seen.contains(&input), "entry {shown:?} appears twice");
        write_part(work, input, &mut entry)?;
        seen.push(input);
    }
    for input in Input::ALL {
        ensure!(seen.contains(&input), "the archive has no {}", input.name());
    }
    for input in Input::ALL {
        fs::rename(input.part(work), input.path(work))
            .with_context(|| format!("cannot place {}", input.name()))?;
    }
    mark_ready(work).context("cannot mark the job ready")
}

/// Mark the job ready, with this process's id: `stage` waits for this
/// process to have exited before its container ends, which would otherwise
/// take it down before the dispatcher hears that it succeeded. The mark
/// appears whole, under its name, or not at all.
fn mark_ready(work: &Work) -> std::io::Result<()> {
    let part = work.input().join(".ready.part");
    let _ = fs::remove_file(&part);
    create(&part)?.write_all(format!("{}\n", std::process::id()).as_bytes())?;
    fs::rename(&part, work.ready())
}

fn write_part(work: &Work, input: Input, from: &mut impl Read) -> Result<()> {
    let path = input.part(work);
    let _ = fs::remove_file(&path);
    let mut file = create(&path).with_context(|| format!("cannot create {}", path.display()))?;
    pump(from, &mut file).with_context(|| format!("cannot write {}", input.name()))?;
    Ok(())
}

/// A new file, mode 0644, that must not exist (and so is not a link).
fn create(path: impl AsRef<std::path::Path>) -> std::io::Result<fs::File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::run;
    use aurcache_build_agent::layout::Work;
    use std::io::Cursor;
    use tar::{Builder, EntryType, Header};

    fn entry(builder: &mut Builder<Vec<u8>>, kind: EntryType, name: &str, data: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        // `set_path` refuses `..`, which is exactly what is being tried.
        let bytes = name.as_bytes();
        header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        if kind == EntryType::Symlink {
            header.set_link_name("/etc/passwd").unwrap();
        }
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }

    fn tar(entries: &[(EntryType, &str, &[u8])]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        for (kind, name, data) in entries {
            entry(&mut builder, *kind, name, data);
        }
        builder.into_inner().unwrap()
    }

    fn files(work: &Work) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(work.input())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    const FILE: EntryType = EntryType::Regular;

    #[test]
    fn both_inputs_land_and_the_job_is_marked_ready() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        let input = tar(&[(FILE, "source.tar.gz", b"src"), (FILE, "spec.json", b"{}")]);
        run(&work, Cursor::new(input)).unwrap();
        assert_eq!(std::fs::read(work.spec()).unwrap(), b"{}");
        assert_eq!(std::fs::read(work.source()).unwrap(), b"src");
        assert_eq!(files(&work), ["ready", "source.tar.gz", "spec.json"]);
    }

    #[test]
    fn anything_else_is_refused_and_leaves_nothing() {
        let hostile: [&[(EntryType, &str, &[u8])]; 6] = [
            &[
                (FILE, "spec.json", b"{}"),
                (FILE, "source.tar.gz", b"s"),
                (FILE, "extra", b""),
            ],
            &[
                (FILE, "spec.json", b"{}"),
                (EntryType::Symlink, "source.tar.gz", b""),
            ],
            &[(FILE, "spec.json", b"{}"), (FILE, "../x", b"")],
            &[(FILE, "spec.json", b"{}"), (FILE, "spec.json", b"{}")],
            &[(FILE, "spec.json", b"{}")],
            &[(FILE, "./spec.json", b"{}"), (FILE, "source.tar.gz", b"s")],
        ];
        for entries in hostile {
            let dir = tempfile::tempdir().unwrap();
            let work = Work::new(dir.path());
            assert!(
                run(&work, Cursor::new(tar(entries))).is_err(),
                "{entries:?}"
            );
            assert_eq!(files(&work), Vec::<String>::new(), "{entries:?}");
        }
    }

    #[test]
    fn a_job_cannot_be_handed_in_twice() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        let input = tar(&[(FILE, "spec.json", b"{}"), (FILE, "source.tar.gz", b"s")]);
        run(&work, Cursor::new(input.clone())).unwrap();
        assert!(run(&work, Cursor::new(input)).is_err());
        assert_eq!(std::fs::read(work.spec()).unwrap(), b"{}");
    }
}
