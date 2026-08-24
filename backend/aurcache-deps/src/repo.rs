use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use alpm_compress::tarball::TarballReader;
use url::Url;

use crate::client::AurClient;
use crate::deps::parse_dep;
use crate::model::Error;

const OFFICIAL_REPO_NAMES: &[&str] = &["core", "extra", "multilib"];
const OFFICIAL_REPO_CACHE_TTL_SECS: u64 = 60 * 60;

pub(crate) fn default_repo_root() -> PathBuf {
    std::env::var("AURCACHE_REPO_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./repo"))
}

pub(crate) fn default_official_mirrorlist_path() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_MIRRORLIST_PATH") {
        return PathBuf::from(path);
    }

    let base = std::env::var("MIRRORLIST_PATH_X86_64")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./config/pacman_x86_64"));
    base.join("mirrorlist")
}

pub(crate) fn default_official_repo_cache_dir() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_REPO_CACHE_DIR") {
        return PathBuf::from(path);
    }

    default_official_mirrorlist_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("official_repo_cache")
}

impl AurClient {
    pub(crate) fn local_repo_dependency_exists(&self, dep_name: &str) -> Result<bool, Error> {
        if !self.repo_root.exists() {
            return Ok(false);
        }

        let mut archives = Vec::new();
        for entry in fs::read_dir(&self.repo_root).map_err(|e| Error::Rpc(e.to_string()))? {
            let entry = entry.map_err(|e| Error::Rpc(e.to_string()))?;
            archives.push(entry.path().join("repo.db.tar.gz"));
        }

        any_archive_provides(archives, dep_name)
    }

    pub(crate) async fn cached_official_dependency_exists(
        &self,
        dep_name: &str,
    ) -> Result<bool, Error> {
        self.refresh_official_repo_cache_if_needed().await?;
        let archives = OFFICIAL_REPO_NAMES.iter().map(|repo_name| {
            self.official_repo_cache_dir
                .join(cache_file_name(repo_name))
        });
        any_archive_provides(archives, dep_name)
    }

    async fn refresh_official_repo_cache_if_needed(&self) -> Result<(), Error> {
        fs::create_dir_all(&self.official_repo_cache_dir).map_err(|e| Error::Rpc(e.to_string()))?;
        let mirrors = mirror_servers(&self.official_mirrorlist_path)?;
        if mirrors.is_empty() {
            return Err(Error::Rpc(
                "No official repo mirrors configured".to_string(),
            ));
        }

        for repo_name in OFFICIAL_REPO_NAMES {
            let archive_path = self
                .official_repo_cache_dir
                .join(cache_file_name(repo_name));
            if !cache_is_stale(&archive_path)? {
                continue;
            }
            self.download_official_repo_db(&mirrors, repo_name, &archive_path)
                .await?;
        }

        Ok(())
    }

    async fn download_official_repo_db(
        &self,
        mirrors: &[String],
        repo_name: &str,
        archive_path: &Path,
    ) -> Result<(), Error> {
        let mut last_error = None;
        for mirror in mirrors {
            let url = official_repo_db_url(mirror, repo_name)?;
            match self.download_to_path(&url, archive_path).await {
                Ok(()) => return Ok(()),
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error
            .unwrap_or_else(|| Error::Rpc("Failed to download official repo db".to_string())))
    }

    /// Downloads `url` to `archive_path`, disabling reqwest's transparent gzip
    /// decompression (via `Accept-Encoding: identity`) so the bytes written to
    /// disk match the wire format of an Arch repo DB (gzip, `.db` served as
    /// `.db.tar.gz`). Without this, reqwest's `gzip` feature auto-decompresses
    /// the response body while the file is still named/expected as `.tar.gz`,
    /// causing `TarballReader` to try to gunzip already-plain tar bytes and
    /// silently fail every lookup against the cache.
    async fn download_to_path(&self, url: &Url, archive_path: &Path) -> Result<(), Error> {
        let bytes = self
            .http
            .get(url.clone())
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(Error::Http)?
            .error_for_status()
            .map_err(Error::Http)?
            .bytes()
            .await
            .map_err(Error::Http)?;
        fs::write(archive_path, bytes).map_err(|e| Error::Rpc(e.to_string()))?;
        Ok(())
    }
}

fn cache_is_stale(path: &Path) -> Result<bool, Error> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(Error::Rpc(err.to_string())),
    };
    let modified = metadata.modified().map_err(|e| Error::Rpc(e.to_string()))?;
    let age = SystemTime::now()
        .duration_since(modified)
        .map_err(|e| Error::Rpc(e.to_string()))?;
    Ok(age.as_secs() > OFFICIAL_REPO_CACHE_TTL_SECS)
}

fn mirror_servers(path: &Path) -> Result<Vec<String>, Error> {
    let content = fs::read_to_string(path).map_err(|e| Error::Rpc(e.to_string()))?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("Server = "))
        .map(str::trim)
        .map(ToString::to_string)
        .collect())
}

fn official_repo_db_url(mirror: &str, repo_name: &str) -> Result<Url, Error> {
    let base = mirror
        .replace("$repo", repo_name)
        .replace("$arch", "x86_64");
    let separator = if base.ends_with('/') { "" } else { "/" };
    Url::parse(&format!("{base}{separator}{repo_name}.db")).map_err(|e| Error::Rpc(e.to_string()))
}

fn cache_file_name(repo_name: &str) -> String {
    format!("{repo_name}.db.tar.gz")
}

/// Returns true if any of the given `repo.db`-style archives provides `dep_name`
/// (by package name or `%PROVIDES%`). Missing archive paths are skipped.
fn any_archive_provides(
    archive_paths: impl IntoIterator<Item = PathBuf>,
    dep_name: &str,
) -> Result<bool, Error> {
    for archive_path in archive_paths {
        if !archive_path.exists() {
            continue;
        }
        if repo_archive_provides(&archive_path, dep_name)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn repo_archive_provides(archive_path: &Path, dep_name: &str) -> Result<bool, Error> {
    let mut reader =
        TarballReader::try_from(archive_path).map_err(|e| Error::Rpc(e.to_string()))?;
    for entry in reader.entries().map_err(|e| Error::Rpc(e.to_string()))? {
        let mut entry = entry.map_err(|e| Error::Rpc(e.to_string()))?;
        if entry.path().file_name().and_then(|name| name.to_str()) != Some("desc") {
            continue;
        }

        let content = String::from_utf8(entry.content().map_err(|e| Error::Rpc(e.to_string()))?)
            .map_err(|e| Error::Rpc(e.to_string()))?;
        if desc_matches_dependency(&content, dep_name) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Parses `name` and `provides` from a desc file content string without strict validation,
/// so it works for both signed and unsigned packages (no %PGPSIG% required).
fn desc_matches_dependency(content: &str, dep_name: &str) -> bool {
    let sections = parse_desc_sections(content);
    let name = sections
        .get("NAME")
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default();
    if name == dep_name {
        return true;
    }
    let provides = sections.get("PROVIDES").cloned().unwrap_or_default();
    provides.iter().any(|p| parse_dep(p).0 == dep_name)
}

/// Extracts all sections from a pacman desc file into a map of section name → values.
/// Each section has the form `%SECTION_NAME%\nval1\nval2\n\n`.
///
/// We parse desc files manually rather than using `alpm-repo-db` because that crate
/// auto-detects the schema version by the presence of `%MD5SUM%`: entries with it are
/// treated as v1, which requires `%PGPSIG%`. AURCache doesn't sign packages, so
/// `%PGPSIG%` is always absent and `alpm-repo-db` would fail on every local repo entry.
/// Since we only need `%NAME%` and `%PROVIDES%` for dependency resolution, a lenient
/// section extractor is both simpler and more robust.
fn parse_desc_sections(content: &str) -> std::collections::HashMap<String, Vec<String>> {
    let mut map = std::collections::HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_values: Vec<String> = Vec::new();

    for line in content.lines() {
        if line.starts_with('%') && line.ends_with('%') {
            if let Some(key) = current_key.take() {
                map.insert(
                    key,
                    current_values.drain(..).filter(|v| !v.is_empty()).collect(),
                );
            }
            current_key = Some(line[1..line.len() - 1].to_string());
        } else if current_key.is_some() {
            current_values.push(line.to_string());
        }
    }
    if let Some(key) = current_key.take() {
        map.insert(
            key,
            current_values
                .into_iter()
                .filter(|v| !v.is_empty())
                .collect(),
        );
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Builds a gzip-compressed tar archive (matching the real wire format of
    /// Arch repo DBs, e.g. `core.db.tar.gz`) containing a single package
    /// `desc` entry for `pkg_name`, optionally providing `provides_name`.
    fn build_repo_db_tar_gz(pkg_name: &str, provides_name: Option<&str>) -> Vec<u8> {
        let mut desc = format!("%NAME%\n{pkg_name}\n\n%VERSION%\n1.0-1\n\n");
        if let Some(provides) = provides_name {
            desc.push_str(&format!("%PROVIDES%\n{provides}\n\n"));
        }

        let gz = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar_builder = Builder::new(gz);
        let entry_path = format!("{pkg_name}-1.0-1/desc");
        let mut header = Header::new_gnu();
        header.set_size(desc.len() as u64);
        header.set_cksum();
        tar_builder
            .append_data(&mut header, &entry_path, desc.as_bytes())
            .unwrap();
        let gz = tar_builder.into_inner().unwrap();
        gz.finish().unwrap()
    }

    /// Regression test for the reqwest gzip auto-decompression bug: the
    /// official Arch mirrors serve `core.db`/`extra.db`/`multilib.db` as
    /// statically pre-gzipped files. If reqwest's `gzip` feature is enabled
    /// (as it is workspace-wide via other crates) and `download_to_path`
    /// doesn't explicitly disable transparent decompression, the
    /// already-decompressed (plain tar) bytes get written to disk under a
    /// `.tar.gz` filename, and `TarballReader` (which selects its
    /// decompression algorithm by file extension) fails to read them back,
    /// making every official-repo dependency lookup silently report "not
    /// found".
    ///
    /// Rather than relying on the `gzip` cargo feature actually being enabled
    /// for this crate in isolation (which depends on feature unification with
    /// other workspace crates, and so isn't reliably exercised by `cargo test
    /// -p aurcache-deps`), this test asserts directly on the outgoing
    /// request: `download_to_path` must send `Accept-Encoding: identity` to
    /// explicitly opt out of any transparent decompression, regardless of
    /// which reqwest features happen to be compiled in. The mock only
    /// responds to requests carrying that header, so this test fails
    /// (download error propagates) if the header is ever removed.
    #[tokio::test]
    async fn download_to_path_disables_transparent_decompression() {
        let server = MockServer::start().await;
        let body = build_repo_db_tar_gz("git", None);

        for repo_name in OFFICIAL_REPO_NAMES {
            Mock::given(method("GET"))
                .and(path(format!("/{repo_name}/os/x86_64/{repo_name}.db")))
                .and(header("Accept-Encoding", "identity"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(body.clone(), "application/octet-stream")
                        .append_header("Content-Encoding", "x-gzip"),
                )
                .mount(&server)
                .await;
        }

        let mirrorlist_dir = tempfile::tempdir().unwrap();
        let mirrorlist_path = mirrorlist_dir.path().join("mirrorlist");
        fs::write(
            &mirrorlist_path,
            format!("Server = {}/$repo/os/$arch\n", server.uri()),
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = AurClient::with_urls_and_paths(
            "http://unused.invalid/rpc/v5",
            tempfile::tempdir().unwrap().path().to_path_buf(),
            mirrorlist_path,
            cache_dir.path().to_path_buf(),
        );

        assert!(
            client
                .cached_official_dependency_exists("git")
                .await
                .expect(
                    "download_to_path must send Accept-Encoding: identity; \
                     otherwise the mock rejects the request (404) and the \
                     lookup fails"
                ),
            "expected 'git' to be found in the cached official repo DBs"
        );

        // Sanity check: the cached archive on disk is genuinely gzip-compressed
        // (matching its `.tar.gz` extension), not silently auto-decompressed.
        let cached_path = cache_dir.path().join(cache_file_name("core"));
        let on_disk = fs::read(&cached_path).unwrap();
        assert_eq!(
            &on_disk[0..2],
            &[0x1f, 0x8b],
            "cached archive should be gzip-compressed on disk (gzip magic bytes)"
        );

        assert!(
            !client
                .cached_official_dependency_exists("not-a-real-package")
                .await
                .unwrap()
        );
    }
}
