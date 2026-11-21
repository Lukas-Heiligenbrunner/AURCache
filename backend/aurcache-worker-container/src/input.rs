//! The job handed in: the spec and the source archive, as the agent reads
//! them.

use anyhow::{Context, Result};
use aurcache_build_agent::layout::{INPUT_SOURCE, INPUT_SPEC, Work};
use aurcache_build_agent::spec::BuildSpec;
use aurcache_common::fs::write_public;
use std::fs;
use std::path::Path;

/// `<work>/in/spec.json` and `<work>/in/source.tar.gz`, for a worker that
/// shares a directory with its build container.
pub fn write(work: &Path, spec: &BuildSpec, source: &[u8]) -> Result<()> {
    let input = Work::new(work).input();
    fs::create_dir_all(&input).with_context(|| format!("creating {}", input.display()))?;
    put(&input.join(INPUT_SPEC), &to_json(spec)?)?;
    put(&input.join(INPUT_SOURCE), source)
}

/// The same two as one tar, for a worker that reaches its build container
/// only through `exec`: what the agent's `receive` reads from its standard
/// input. `source` is moved in and is the middle of the archive as it is, so
/// the source is held once, never copied into an archive of its own.
pub fn pack(spec: &BuildSpec, source: Vec<u8>) -> Result<Packed> {
    let spec = to_json(spec)?;
    let mut head = Vec::with_capacity(3 * BLOCK + spec.len());
    head.extend_from_slice(header(INPUT_SPEC, spec.len())?.as_bytes());
    head.extend_from_slice(&spec);
    head.resize(head.len() + padding(spec.len()), 0);
    head.extend_from_slice(header(INPUT_SOURCE, source.len())?.as_bytes());
    let mut tail = vec![0; padding(source.len())];
    tail.resize(tail.len() + END, 0);
    Ok(Packed { head, source, tail })
}

/// A tar of the spec and the source in three pieces, which written one after
/// the other are the archive: everything before the source's bytes, the
/// source itself, and everything after them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Packed {
    /// The spec's entry, and the source's header.
    pub head: Vec<u8>,
    pub source: Vec<u8>,
    /// The source's padding to a whole block, and the end-of-archive marker.
    pub tail: Vec<u8>,
}

impl Packed {
    /// The pieces, in the order they are written.
    #[must_use]
    pub fn pieces(&self) -> [&[u8]; 3] {
        [&self.head, &self.source, &self.tail]
    }
}

/// A tar block.
const BLOCK: usize = 512;
/// The end-of-archive marker: two zero blocks.
const END: usize = 2 * BLOCK;

/// The zeros that take `len` bytes of data to a whole block.
fn padding(len: usize) -> usize {
    (BLOCK - len % BLOCK) % BLOCK
}

/// The header of a regular file `name` of `len` bytes, anyone may read.
fn header(name: &str, len: usize) -> Result<tar::Header> {
    let mut header = tar::Header::new_gnu();
    header.set_size(len as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header
        .set_path(name)
        .with_context(|| format!("naming {name} in the input archive"))?;
    header.set_cksum();
    Ok(header)
}

fn to_json(spec: &BuildSpec) -> Result<Vec<u8>> {
    serde_json::to_vec(spec).context("encoding the build spec")
}

/// A file anyone may read, since the agent reads it as another user.
fn put(path: &Path, bytes: &[u8]) -> Result<()> {
    write_public(path, bytes).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_build_agent::spec::INTERFACE;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    fn spec() -> BuildSpec {
        BuildSpec {
            interface: INTERFACE,
            pkgbase: "hello".into(),
            build_flags: vec!["--nocheck".into()],
            makepkg_conf: "PACKAGER=x\n".into(),
            pacman_conf: "[options]\n".into(),
            pacman_upgrade_conf: "[options]\n".into(),
            mirrorlist: None,
            pgp_keys: vec![],
            keyserver: "hkps://keys.example".into(),
            vcs_sources: vec![],
            cpus: Some(2.0),
            scratch: "/scratch".into(),
            ssh: None,
        }
    }

    struct Entry {
        name: String,
        mode: u32,
        bytes: Vec<u8>,
    }

    fn entries(archive: &[u8]) -> Vec<Entry> {
        tar::Archive::new(archive)
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                Entry {
                    name: entry.path().unwrap().to_string_lossy().into_owned(),
                    mode: entry.header().mode().unwrap(),
                    bytes,
                }
            })
            .collect()
    }

    /// The pieces, one after the other, as `receive` reads them.
    fn joined(packed: &Packed) -> Vec<u8> {
        packed.pieces().concat()
    }

    /// The archive as `tar` itself writes it, entry by entry.
    fn built(spec: &BuildSpec, source: &[u8]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, bytes) in [
            (INPUT_SPEC, to_json(spec).unwrap()),
            (INPUT_SOURCE, source.to_vec()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            tar.append_data(&mut header, name, bytes.as_slice())
                .unwrap();
        }
        tar.into_inner().unwrap()
    }

    #[test]
    fn pack_is_exactly_the_two_entries() {
        let source = b"\x1f\x8bnot really gzip";
        let found = entries(&joined(&pack(&spec(), source.to_vec()).unwrap()));
        let names: Vec<_> = found.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, [INPUT_SPEC, INPUT_SOURCE]);
        let decoded: BuildSpec = serde_json::from_slice(&found[0].bytes).unwrap();
        assert_eq!(decoded, spec());
        assert_eq!(found[1].bytes, source);
        assert!(found.iter().all(|e| e.mode == 0o644));
    }

    #[test]
    fn write_produces_the_same_two_files() {
        let work = tempfile::tempdir().unwrap();
        write(work.path(), &spec(), b"source").unwrap();
        let input = work.path().join("in");
        for entry in entries(&joined(&pack(&spec(), b"source".to_vec()).unwrap())) {
            let file = input.join(&entry.name);
            assert_eq!(fs::read(&file).unwrap(), entry.bytes, "{}", entry.name);
            let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644);
        }
        let mut listed: Vec<_> = fs::read_dir(&input)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        listed.sort();
        assert_eq!(listed, [INPUT_SOURCE, INPUT_SPEC]);
    }

    #[test]
    fn write_replaces_what_was_there() {
        let work = tempfile::tempdir().unwrap();
        write(work.path(), &spec(), b"first, and longer").unwrap();
        write(work.path(), &spec(), b"second").unwrap();
        let source = fs::read(work.path().join("in").join(INPUT_SOURCE)).unwrap();
        assert_eq!(source, b"second");
    }

    #[test]
    fn an_empty_source_is_still_an_entry() {
        let found = entries(&joined(&pack(&spec(), Vec::new()).unwrap()));
        assert_eq!(found.len(), 2);
        assert!(found[1].bytes.is_empty());
    }

    /// Byte for byte the archive `tar` writes, whatever the source's length
    /// is to a block, and the source is the middle piece untouched.
    #[test]
    fn the_pieces_are_the_archive_tar_writes() {
        for len in [0, 1, 511, 512, 513, 4096, 70_000] {
            let source: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let packed = pack(&spec(), source.clone()).unwrap();
            assert_eq!(packed.source, source, "for {len}");
            assert_eq!(joined(&packed), built(&spec(), &source), "for {len}");
        }
    }
}
