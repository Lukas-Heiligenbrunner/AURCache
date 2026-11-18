//! The job handed in: the spec and the source archive, as the agent reads
//! them.

use anyhow::{Context, Result};
use aurcache_build_agent::layout::{INPUT_SOURCE, INPUT_SPEC, Work};
use aurcache_build_agent::spec::BuildSpec;
use aurcache_common::fs::write_public;
use std::fs;
use std::path::Path;

/// `<work>/in/spec.json` and `<work>/in/source.tar.gz`, for a worker that shares a directory.
pub fn write(work: &Path, spec: &BuildSpec, source: &[u8]) -> Result<()> {
    let input = Work::new(work).input();
    fs::create_dir_all(&input).with_context(|| format!("creating {}", input.display()))?;
    put(&input.join(INPUT_SPEC), &to_json(spec)?)?;
    put(&input.join(INPUT_SOURCE), source)
}

/// The same two as the tar `receive` reads.
pub fn pack(spec: &BuildSpec, source: &[u8]) -> Result<Vec<u8>> {
    let mut tar = tar::Builder::new(Vec::new());
    add(&mut tar, INPUT_SPEC, &to_json(spec)?)?;
    add(&mut tar, INPUT_SOURCE, source)?;
    tar.into_inner().context("finishing the input archive")
}

fn to_json(spec: &BuildSpec) -> Result<Vec<u8>> {
    serde_json::to_vec(spec).context("encoding the build spec")
}

/// A file anyone may read, since the agent reads it as another user.
fn put(path: &Path, bytes: &[u8]) -> Result<()> {
    write_public(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn add(tar: &mut tar::Builder<Vec<u8>>, name: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    tar.append_data(&mut header, name, bytes)
        .with_context(|| format!("adding {name} to the input archive"))
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

    #[test]
    fn pack_is_exactly_the_two_entries() {
        let source = b"\x1f\x8bnot really gzip";
        let found = entries(&pack(&spec(), source).unwrap());
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
        for entry in entries(&pack(&spec(), b"source").unwrap()) {
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
        let found = entries(&pack(&spec(), b"").unwrap());
        assert_eq!(found.len(), 2);
        assert!(found[1].bytes.is_empty());
    }
}
