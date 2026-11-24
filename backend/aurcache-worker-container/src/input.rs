//! The job handed in: the spec and the source archive, as the agent reads
//! them.

use anyhow::{Context, Result};
use aurcache_build_agent::handin::prefix;
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

/// The same two as the stream that the agent's `receive` reads, for a worker
/// that reaches its build container only through `exec`. `source` is moved
/// in, never copied.
pub fn pack(spec: &BuildSpec, source: Vec<u8>) -> Result<Packed> {
    let spec = to_json(spec)?;
    let mut head = prefix(spec.len()).into_bytes();
    head.extend_from_slice(&spec);
    head.extend_from_slice(prefix(source.len()).as_bytes());
    Ok(Packed { head, source })
}

/// The stream that `receive` reads, as two pieces written in order.
#[derive(Debug, Default)]
pub struct Packed {
    /// The spec, and the source's length.
    pub head: Vec<u8>,
    pub source: Vec<u8>,
}

impl Packed {
    /// The pieces, in the order they are written.
    #[must_use]
    pub fn pieces(&self) -> [&[u8]; 2] {
        [&self.head, &self.source]
    }
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
    use std::os::unix::fs::PermissionsExt;

    fn spec() -> BuildSpec {
        BuildSpec {
            interface: INTERFACE,
            pkgbase: "hello".into(),
            arch: "x86_64".into(),
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

    #[test]
    fn write_produces_the_two_files_anyone_may_read() {
        let work = tempfile::tempdir().unwrap();
        write(work.path(), &spec(), b"source").unwrap();
        let input = work.path().join("in");
        let decoded: BuildSpec =
            serde_json::from_slice(&fs::read(input.join(INPUT_SPEC)).unwrap()).unwrap();
        assert_eq!(decoded, spec());
        assert_eq!(fs::read(input.join(INPUT_SOURCE)).unwrap(), b"source");
        for name in [INPUT_SPEC, INPUT_SOURCE] {
            let mode = fs::metadata(input.join(name)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "{name}");
        }
        let mut listed: Vec<_> = fs::read_dir(&input)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        listed.sort();
        assert_eq!(listed, [INPUT_SOURCE, INPUT_SPEC]);
    }

    #[test]
    fn pack_is_the_spec_then_the_source_each_after_its_length() {
        let packed = pack(&spec(), b"source".to_vec()).unwrap();
        let stream = packed.pieces().concat();
        let mut reader = stream.as_slice();
        let len = aurcache_build_agent::handin::read_len(&mut reader).unwrap();
        let (json, mut rest) = reader.split_at(usize::try_from(len).unwrap());
        assert_eq!(serde_json::from_slice::<BuildSpec>(json).unwrap(), spec());
        assert_eq!(
            aurcache_build_agent::handin::read_len(&mut rest).unwrap(),
            6
        );
        assert_eq!(rest, b"source");
        assert_eq!(packed.source, b"source");
    }

    #[test]
    fn write_replaces_what_was_there() {
        let work = tempfile::tempdir().unwrap();
        write(work.path(), &spec(), b"first").unwrap();
        write(work.path(), &spec(), b"").unwrap();
        let source = fs::read(work.path().join("in").join(INPUT_SOURCE)).unwrap();
        assert!(source.is_empty());
    }
}
