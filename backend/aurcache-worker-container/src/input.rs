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
            cpus: Some(2.0),
            scratch: "/scratch".into(),
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
    fn write_replaces_what_was_there() {
        let work = tempfile::tempdir().unwrap();
        write(work.path(), &spec(), b"first").unwrap();
        write(work.path(), &spec(), b"").unwrap();
        let source = fs::read(work.path().join("in").join(INPUT_SOURCE)).unwrap();
        assert!(source.is_empty());
    }
}
