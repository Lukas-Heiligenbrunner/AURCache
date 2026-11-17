//! What a build's `.SRCINFO` says about its need for the worker's SSH key.
//!
//! The SSH window opens only for a build with a source that is fetched over
//! SSH: a build with none never has the socket, and nothing of it, not even
//! the package's own top level, can ask the key for a signature.

/// Whether any `source` or `source_<arch>` entry of `srcinfo` (what
/// `makepkg --printsrcinfo` prints) is fetched over SSH.
#[must_use]
pub fn needs_ssh(srcinfo: &str) -> bool {
    srcinfo.lines().filter_map(source_value).any(over_ssh)
}

/// The value of a `source = ...` or `source_<arch> = ...` line.
fn source_value(line: &str) -> Option<&str> {
    let (key, value) = line.trim().split_once('=')?;
    let key = key.trim();
    (key == "source" || key.starts_with("source_")).then(|| value.trim())
}

/// Whether a source entry is one makepkg or its VCS client reaches over SSH:
/// a scheme with `ssh`, `sftp` or `scp` in it (`git+ssh://`, `ssh://`,
/// `hg+ssh://`, `scp://`, ...), or a `user@host:path` remote after a VCS
/// prefix, which git reads as SSH.
fn over_ssh(entry: &str) -> bool {
    // `name::url` names the file the source is saved as.
    let url = entry.split_once("::").map_or(entry, |(_, url)| url);
    if let Some((scheme, _)) = url.split_once("://") {
        return scheme
            .split('+')
            .any(|part| matches!(part, "ssh" | "sftp" | "scp"));
    }
    let remote = url.split_once('+').map_or(url, |(_, rest)| rest);
    match remote.split_once(':') {
        Some((host, _)) => host.contains('@') && !host.contains('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::needs_ssh;

    fn srcinfo(sources: &[&str]) -> String {
        let mut text = "pkgbase = demo\n\tpkgver = 1\n".to_string();
        for source in sources {
            text.push_str(&format!("\tsource = {source}\n"));
        }
        text.push_str("\npkgname = demo\n");
        text
    }

    #[test]
    fn ssh_sources_are_seen() {
        for source in [
            "payload::git+ssh://git@example.test/srv/git/payload.git",
            "git+ssh://git@github.com/org/repo.git#branch=main",
            "ssh://example.test/file",
            "hg+ssh://hg@example.test/repo",
            "svn+ssh://example.test/repo",
            "scp://example.test/file.tar.gz",
            "sftp://example.test/file.tar.gz",
            "repo::git+git@github.com:org/repo.git",
        ] {
            assert!(needs_ssh(&srcinfo(&[source])), "{source}");
        }
        let arch = "pkgbase = demo\n\tsource_x86_64 = git+ssh://git@example.test/x.git\n";
        assert!(needs_ssh(arch));
    }

    #[test]
    fn other_sources_are_not() {
        assert!(!needs_ssh(&srcinfo(&[])));
        for source in [
            "https://example.test/file.tar.gz",
            "git+https://github.com/org/repo.git",
            "fixture::git+file:///srv/upstream.git",
            "local.patch",
            "name::https://example.test/a:b",
            "https://user@example.test/file",
        ] {
            assert!(!needs_ssh(&srcinfo(&[source])), "{source}");
        }
        // Only source lines count.
        assert!(!needs_ssh(
            "\turl = ssh://example.test\n\tsource = a.patch\n"
        ));
    }
}
