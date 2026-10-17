use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use alpm_compress::tarball::TarballReader;
use reqwest::Client;
use tokio::sync::RwLock;
use url::Url;

use crate::client::send_with_retry;
use crate::model::Error;

const OFFICIAL_REPO_NAMES: [&str; 3] = ["core", "extra", "multilib"];
const OFFICIAL_REPO_CACHE_TTL_SECS: u64 = 60 * 60;
/// A mirror that accepts the connection and then stalls never fails on its
/// own, and there is nothing to retry until the first attempt returns. This is
/// what bounds that.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Mirrorlist used to locate official repo databases.
///
/// Official repo DBs are fetched for `x86_64` only today (see
/// [`official_repo_db_url`], which substitutes `$arch` with `x86_64`), so this
/// asks for that architecture explicitly rather than guessing the host's.
/// `OFFICIAL_MIRRORLIST_PATH` still overrides it outright.
///
/// This resolves through [`crate::paths`] so it cannot drift from where the
/// mirrorlist is actually written — it previously defaulted to
/// `./config/pacman_x86_64/mirrorlist`, a path nothing ever wrote, which made
/// dependency resolution fail for every package that had dependencies.
pub(crate) fn default_official_mirrorlist_path() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_MIRRORLIST_PATH") {
        return PathBuf::from(path);
    }
    crate::paths::mirrorlist_path("x86_64")
}

pub(crate) fn default_official_repo_cache_dir() -> PathBuf {
    crate::paths::official_repo_cache_dir()
}

/// The official repositories, as AURCache is able to see them.
///
/// One type owns the whole interaction: where the databases are fetched from,
/// the cached copies on disk, how stale they may be, and the names read out of
/// them. Callers ask [`OfficialRepos::holds`] and learn nothing about which of
/// those answered, which is the point -- the answer has to be the same either
/// way.
///
/// The names are kept in memory because reading them is not cheap: `extra.db`
/// alone is ~9 MiB of gzip over ~15k packages, and dependency resolution runs
/// once per package an add plans. Reading it per resolution meant an add of a
/// hundred-dependency package spent most of its time in gunzip.
pub struct OfficialRepos {
    http: Client,
    mirrorlist_path: PathBuf,
    cache_dir: PathBuf,
    /// One set of names per database, in [`OFFICIAL_REPO_NAMES`] order and
    /// sized from it, so the two cannot come apart. `None` until the first
    /// successful refresh: the server serves while it retries, so questions
    /// can arrive before there is anything to answer them with.
    names: RwLock<Option<[HashSet<String>; OFFICIAL_REPO_NAMES.len()]>>,
}

impl std::fmt::Debug for OfficialRepos {
    /// Where the databases come from and how much was read from each -- never
    /// the sets, which are ~22k names.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.names.try_read().ok().map(|names| {
            names
                .as_ref()
                .map(|held| held.iter().map(HashSet::len).collect::<Vec<_>>())
        });
        f.debug_struct("OfficialRepos")
            .field("mirrorlist_path", &self.mirrorlist_path)
            .field("cache_dir", &self.cache_dir)
            .field("names_held", &held)
            .finish()
    }
}

impl OfficialRepos {
    pub(crate) fn new(http: Client, mirrorlist_path: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            http,
            mirrorlist_path,
            cache_dir,
            names: RwLock::new(None),
        }
    }

    /// Whether the official repositories hold `name` -- by package name or by
    /// `provides`.
    ///
    /// Answers from memory or not at all. Reporting "not in the official
    /// repositories" when the truth is "could not ask" is how every ordinary
    /// `core`/`extra` name ends up resolved against the AUR, where a
    /// `provides` search can turn `git` into `git-git`.
    pub async fn holds(&self, name: &str) -> Result<bool, Error> {
        match self.names.read().await.as_ref() {
            Some(held) => Ok(held.iter().any(|database| database.contains(name))),
            None => Err(Error::OfficialReposUnread),
        }
    }

    /// Make every database current and rebuild the names read from it.
    ///
    /// Run on a short tick: the TTL decides what a tick *does*, so downloads
    /// stay hourly while a failed refresh is retried within a minute. A tick
    /// with nothing to do costs three `stat`s.
    ///
    /// The three are handled in turn rather than concurrently: `extra` is
    /// 8.7 MiB of the 8.9 and sets the pace either way, and this runs in the
    /// background once an hour.
    pub async fn refresh(&self) -> Result<(), Error> {
        fs::create_dir_all(&self.cache_dir)?;

        let stale = self.stale_databases()?;
        let held = self.names.read().await;
        if stale.is_empty() && held.is_some() {
            return Ok(());
        }

        // Cloned so the read lock is released before the download: a refresh
        // must not stop resolutions answering from what is already here.
        let current = held.clone();
        drop(held);

        let mirrors = if stale.is_empty() {
            Vec::new()
        } else {
            let mirrors = official_mirror_servers(&self.mirrorlist_path)?;
            if mirrors.is_empty() {
                return Err(Error::Rpc(
                    "No official repo mirrors configured".to_string(),
                ));
            }
            mirrors
        };

        let mut rebuilt = Vec::with_capacity(OFFICIAL_REPO_NAMES.len());
        for index in 0..OFFICIAL_REPO_NAMES.len() {
            rebuilt.push(
                self.refresh_one(&mirrors, index, stale.contains(&index), &current)
                    .await?,
            );
        }

        // The write lock is taken only to move the finished sets into place: a
        // reader waits for three `HashSet`s to be swapped, never for a mirror.
        let mut names = self.names.write().await;
        *names = Some(rebuilt.try_into().expect("one set per database"));
        Ok(())
    }

    /// Bring one database up to date and read the names out of it.
    ///
    /// Three outcomes, named rather than left to fall out of the control flow:
    /// a stale database is downloaded and the bytes just written are parsed; a
    /// current one already in memory is kept as it is, which is what stops a
    /// `core` refresh re-reading 8.7 MiB of `extra`; a current one not in
    /// memory is read from disk, which is what a restart does.
    ///
    /// Downloading and reading are one step because they are one fact: the
    /// names in memory are the names in the file just written, and nothing
    /// between the two can leave them disagreeing. The file is read back
    /// rather than parsed from the buffer because `TarballReader` chooses its
    /// decompressor from the extension, so it wants a path -- and the bytes
    /// are in the page cache by then.
    async fn refresh_one(
        &self,
        mirrors: &[String],
        index: usize,
        stale: bool,
        current: &Option<[HashSet<String>; OFFICIAL_REPO_NAMES.len()]>,
    ) -> Result<HashSet<String>, Error> {
        let repo_name = OFFICIAL_REPO_NAMES[index];
        let path = self.database_path(index);

        if !stale && let Some(held) = current {
            return Ok(held[index].clone());
        }

        if stale {
            let bytes = self.download(mirrors, repo_name).await?;
            fs::write(&path, &bytes)?;
        }

        // Absent means someone removed it after `stale_databases` said it was
        // current -- not "this repository publishes nothing", which is the
        // answer that gets ordinary packages built from the AUR.
        if !path.exists() {
            return Err(Error::Rpc(format!(
                "the {repo_name} database vanished from {}",
                self.cache_dir.display()
            )));
        }

        let mut held = HashSet::new();
        names_in_archive(&path, &mut held)?;
        Ok(held)
    }

    /// The databases whose cached copy has aged out, or was never fetched.
    fn stale_databases(&self) -> Result<Vec<usize>, Error> {
        (0..OFFICIAL_REPO_NAMES.len())
            .filter_map(|index| match cache_is_stale(&self.database_path(index)) {
                Ok(true) => Some(Ok(index)),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    fn database_path(&self, index: usize) -> PathBuf {
        self.cache_dir
            .join(cache_file_name(OFFICIAL_REPO_NAMES[index]))
    }

    /// Fetch one database, trying each mirror in turn.
    async fn download(&self, mirrors: &[String], repo_name: &str) -> Result<Vec<u8>, Error> {
        let mut last_error = None;
        for mirror in mirrors {
            let url = official_repo_db_url(mirror, repo_name)?;
            match self.download_from(&url).await {
                Ok(bytes) => return Ok(bytes),
                Err(err) => last_error = Some(err),
            }
        }

        // Deliberately fatal. Answering "not found in the official
        // repositories" when the truth is "could not ask" sends every ordinary
        // `core`/`extra` name off to be resolved against the AUR, where a
        // `provides` search can turn `glibc` into something to build.
        Err(last_error.unwrap_or_else(|| {
            Error::Rpc(format!(
                "could not refresh the {repo_name} database from any mirror; \
                 dependency resolution cannot tell what the official \
                 repositories hold"
            ))
        }))
    }

    /// Fetch `url`, retrying a blip on the same policy as the AUR requests.
    ///
    /// `Accept-Encoding: identity` disables reqwest's transparent gzip
    /// decompression, so the bytes match the wire format of an Arch repo DB
    /// (gzip, `.db` served as `.db.tar.gz`). Without it, reqwest's `gzip`
    /// feature decompresses the body while the file is still named and read
    /// as `.tar.gz`, and every lookup against the cache silently reports "not
    /// found".
    async fn download_from(&self, url: &Url) -> Result<Vec<u8>, Error> {
        send_with_retry(|| {
            self.http
                .get(url.clone())
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .timeout(DOWNLOAD_TIMEOUT)
                .send()
        })
        .await
        .map_err(Error::Http)?
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(Error::Http)
    }
}

fn cache_is_stale(path: &Path) -> Result<bool, Error> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(err.into()),
    };
    let modified = metadata.modified()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .map_err(|e| Error::Rpc(e.to_string()))?;
    Ok(age.as_secs() > OFFICIAL_REPO_CACHE_TTL_SECS)
}

/// Mirrors to fetch the official repository *databases* from.
///
/// `OFFICIAL_MIRRORLIST_SERVERS` (a `;`-separated server list, same shape as
/// `MIRRORLIST_SERVERS_X86_64`) takes precedence over the mirrorlist file.
///
/// Separate from what workers are given because the two want different things:
/// this fetches three small `.db` files hourly and wants a mirror that is
/// close and reliable, while a worker bulk-downloads packages and may sit on
/// entirely different hardware. Sharing one setting is a fine default and a
/// poor requirement.
fn official_mirror_servers(path: &Path) -> Result<Vec<String>, Error> {
    if let Ok(servers) = std::env::var("OFFICIAL_MIRRORLIST_SERVERS") {
        let configured: Vec<String> = servers
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        if !configured.is_empty() {
            return Ok(configured);
        }
    }
    mirror_servers(path)
}

fn mirror_servers(path: &Path) -> Result<Vec<String>, Error> {
    let content = fs::read_to_string(path)?;
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
    Ok(Url::parse(&format!("{base}{separator}{repo_name}.db"))?)
}

fn cache_file_name(repo_name: &str) -> String {
    format!("{repo_name}.db.tar.gz")
}

/// Wrap a repo-database decoding failure, keeping the original as the source.
fn repo_db_error(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::RepoDb(Box::new(e))
}

/// Collect every name one database answers to.
fn names_in_archive(archive: &Path, held: &mut HashSet<String>) -> Result<(), Error> {
    let mut reader = TarballReader::try_from(archive).map_err(repo_db_error)?;
    for entry in reader.entries().map_err(repo_db_error)? {
        let mut entry = entry.map_err(repo_db_error)?;
        if entry.path().file_name().and_then(|name| name.to_str()) != Some("desc") {
            continue;
        }

        let content =
            String::from_utf8(entry.content().map_err(repo_db_error)?).map_err(repo_db_error)?;
        names_in_desc(&content, held);
    }
    Ok(())
}

/// Read the names one `desc` entry answers to: its `%NAME%` -- the database
/// holds one entry per package, so a split package has its own -- and each
/// `%PROVIDES%` line, with any `=version` dropped.
///
/// `%BASE%` is deliberately not read: a name the repositories publish resolves
/// to `Available`, which carries no package for a pkgbase to fill in.
///
/// Parsed by hand rather than with `alpm-repo-db`, which auto-detects the
/// schema version by the presence of `%MD5SUM%`: entries with it are treated
/// as v1, which requires `%PGPSIG%`. AURCache does not sign packages, so
/// `%PGPSIG%` is always absent and `alpm-repo-db` would fail on every entry.
/// Two sections out of a dozen are wanted here, so a lenient scan is both
/// simpler and more robust.
fn names_in_desc(content: &str, held: &mut HashSet<String>) {
    enum Section {
        Name,
        Provides,
        Other,
    }

    let mut section = Section::Other;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('%').and_then(|l| l.strip_suffix('%')) {
            section = match header {
                "NAME" => Section::Name,
                "PROVIDES" => Section::Provides,
                _ => Section::Other,
            };
            continue;
        }
        match section {
            Section::Name => {
                held.insert(line.to_string());
            }
            // `provides` is either a bare name or `name=version`; unlike a
            // dependency it never carries an inequality, so splitting on `=`
            // is the whole grammar.
            Section::Provides => {
                let provided = line.split_once('=').map_or(line, |(name, _)| name.trim());
                held.insert(provided.to_string());
            }
            Section::Other => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::fmt::Write as _;
    use tar::{Builder, Header};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Builds a gzip-compressed tar archive (matching the real wire format of
    /// Arch repo DBs, e.g. `core.db.tar.gz`) holding one `desc` entry per
    /// package.
    fn build_repo_db_tar_gz(packages: &[(&str, Option<&str>)]) -> Vec<u8> {
        let gz = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar_builder = Builder::new(gz);

        for (pkg_name, provides_name) in packages {
            let mut desc = format!("%NAME%\n{pkg_name}\n\n%VERSION%\n1.0-1\n\n");
            if let Some(provides) = provides_name {
                let _ = write!(desc, "%PROVIDES%\n{provides}\n\n");
            }
            let mut header = Header::new_gnu();
            header.set_size(desc.len() as u64);
            header.set_cksum();
            tar_builder
                .append_data(
                    &mut header,
                    format!("{pkg_name}-1.0-1/desc"),
                    desc.as_bytes(),
                )
                .unwrap();
        }

        tar_builder.into_inner().unwrap().finish().unwrap()
    }

    fn repos(mirrorlist: PathBuf, cache_dir: PathBuf) -> OfficialRepos {
        OfficialRepos::new(Client::new(), mirrorlist, cache_dir)
    }

    /// Seed all three databases with the same packages.
    fn seed(cache_dir: &Path, packages: &[(&str, Option<&str>)]) {
        fs::create_dir_all(cache_dir).unwrap();
        for repo_name in OFFICIAL_REPO_NAMES {
            fs::write(
                cache_dir.join(cache_file_name(repo_name)),
                build_repo_db_tar_gz(packages),
            )
            .unwrap();
        }
    }

    /// Until something has been read, a question has no answer -- and saying
    /// "no" would be the answer that gets `git` built from the AUR.
    #[tokio::test]
    async fn an_unread_repository_answers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let repos = repos(dir.path().join("no-mirrorlist"), dir.path().join("cache"));

        assert!(matches!(
            repos.holds("git").await,
            Err(Error::OfficialReposUnread)
        ));
    }

    /// A cache inside the TTL is read, not fetched: a restart costs a read and
    /// not three downloads, and no mirrorlist is needed for it.
    #[tokio::test]
    async fn a_fresh_cache_is_read_without_a_mirrorlist() {
        let cache_dir = tempfile::tempdir().unwrap();
        seed(cache_dir.path(), &[("git", None)]);

        let repos = repos(
            PathBuf::from("/nonexistent/mirrorlist"),
            cache_dir.path().to_path_buf(),
        );
        repos.refresh().await.expect("a warm cache needs no mirror");

        assert!(repos.holds("git").await.unwrap());
        assert!(!repos.holds("not-a-real-package").await.unwrap());
    }

    /// A name reached through `%PROVIDES%` counts, and the version it is
    /// provided at is not part of the name.
    #[tokio::test]
    async fn a_provided_name_is_held_under_the_name_alone() {
        let cache_dir = tempfile::tempdir().unwrap();
        seed(
            cache_dir.path(),
            &[("libjpeg-turbo", Some("libjpeg=8.3.2"))],
        );

        let repos = repos(
            PathBuf::from("/nonexistent/mirrorlist"),
            cache_dir.path().to_path_buf(),
        );
        repos.refresh().await.unwrap();

        assert!(repos.holds("libjpeg").await.unwrap());
        assert!(!repos.holds("libjpeg=8.3.2").await.unwrap());
    }

    /// A tick with nothing stale keeps what it has, and does not need the
    /// files it read to still be there.
    #[tokio::test]
    async fn a_tick_with_nothing_stale_keeps_what_it_has() {
        let cache_dir = tempfile::tempdir().unwrap();
        seed(cache_dir.path(), &[("git", None)]);

        let repos = repos(
            PathBuf::from("/nonexistent/mirrorlist"),
            cache_dir.path().to_path_buf(),
        );
        repos.refresh().await.unwrap();

        // If the second refresh re-read the databases rather than keeping the
        // sets it holds, this would fail: they are gone.
        for repo_name in OFFICIAL_REPO_NAMES {
            fs::remove_file(cache_dir.path().join(cache_file_name(repo_name))).unwrap();
            // ... and put back empty, so nothing counts them as stale.
            fs::write(
                cache_dir.path().join(cache_file_name(repo_name)),
                b"garbage",
            )
            .unwrap();
        }
        repos.refresh().await.unwrap();

        assert!(repos.holds("git").await.unwrap());
    }

    /// Regression test for the reqwest gzip auto-decompression bug: the
    /// official Arch mirrors serve `core.db`/`extra.db`/`multilib.db` as
    /// statically pre-gzipped files. If reqwest's `gzip` feature is enabled
    /// (as it is workspace-wide via other crates) and the download does not
    /// explicitly disable transparent decompression, the already-decompressed
    /// (plain tar) bytes are parsed and written under a `.tar.gz` name, and
    /// every lookup silently reports "not found".
    ///
    /// Rather than relying on the `gzip` cargo feature actually being enabled
    /// for this crate in isolation, this asserts on the outgoing request: the
    /// mock answers only requests carrying `Accept-Encoding: identity`, so
    /// removing that header fails the download instead.
    #[tokio::test]
    async fn the_download_disables_transparent_decompression() {
        let server = MockServer::start().await;
        let body = build_repo_db_tar_gz(&[("git", None)]);

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

        let dir = tempfile::tempdir().unwrap();
        let mirrorlist_path = dir.path().join("mirrorlist");
        fs::write(
            &mirrorlist_path,
            format!("Server = {}/$repo/os/$arch\n", server.uri()),
        )
        .unwrap();
        let cache_dir = dir.path().join("cache");

        let repos = repos(mirrorlist_path, cache_dir.clone());
        repos.refresh().await.expect(
            "the download must send Accept-Encoding: identity; otherwise the \
             mock rejects the request and the refresh fails",
        );

        assert!(repos.holds("git").await.unwrap());

        // The cached copy is the wire format its name claims, so a restart can
        // read it back.
        let on_disk = fs::read(cache_dir.join(cache_file_name("core"))).unwrap();
        assert_eq!(
            &on_disk[0..2],
            &[0x1f, 0x8b],
            "cached archive should be gzip-compressed on disk (gzip magic bytes)"
        );
    }

    /// Reading the databases is not allowed to fail quietly. A corrupt one
    /// used to be indistinguishable from "the official repositories do not
    /// have it", which sent ordinary `core` names off to be built.
    #[tokio::test]
    async fn an_unreadable_database_fails_the_refresh() {
        let cache_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(cache_dir.path()).unwrap();
        for repo_name in OFFICIAL_REPO_NAMES {
            fs::write(
                cache_dir.path().join(cache_file_name(repo_name)),
                b"garbage",
            )
            .unwrap();
        }

        let repos = repos(
            PathBuf::from("/nonexistent/mirrorlist"),
            cache_dir.path().to_path_buf(),
        );

        assert!(
            repos.refresh().await.is_err(),
            "a corrupt database must not read as an empty repository"
        );
    }
}
