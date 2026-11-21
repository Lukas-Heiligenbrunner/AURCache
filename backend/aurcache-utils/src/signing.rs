//! Bringing what the repository already publishes under its key.
//!
//! A repository that signs signs every package it publishes from then on
//! ([`Repository::describe`]). What it published before -- unsigned, or
//! signed by a key it no longer uses -- is signed here, through the same
//! steps as publishing: a copy of each file is signed and described before
//! the lock, and an update replaces its entry and moves its signature in. The
//! package files themselves never move. The databases are only ever signed
//! by an update that writes them, which a backfill forces where nothing else
//! would.

use crate::repository::{NotAPackage, Repository, Unsigned};
use aurcache_signing::InstanceKey;
use pacman_mirrors::platforms::Platform;
use pacman_repo_utils::{ListedPackage, PackageEntry};
use sea_orm::DatabaseConnection;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// How many updates a platform's backfill is spread over, at most. Each one
/// writes the platform's databases again, whatever it signs, so a batch
/// grows with the repository rather than its count.
const BATCHES: usize = 8;

/// How many packages an update signs, at least -- all of them, in a smaller
/// repository. Bounds how much a filesystem without hard links copies at
/// once, there.
const MIN_BATCH: usize = 64;

/// What a backfill did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backfill {
    /// Every listed package on disk that can be signed, and every database,
    /// carries the key's signature.
    Complete {
        signed: usize,
        /// The listed files that cannot be signed, as no earlier run of the
        /// same [`Backfiller`] found them: they do not keep a backfill from
        /// being complete.
        skipped: Vec<Skipped>,
    },
    /// Some packages could not be signed this time, each with why.
    Incomplete {
        signed: usize,
        /// As [`Backfill::Complete`]'s.
        skipped: Vec<Skipped>,
        problems: Vec<String>,
    },
}

/// A listed file a backfill leaves unsigned for good: signing it again would
/// fail the same way. Publishing its package again replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Where the file is, or should be.
    pub path: PathBuf,
    pub why: Unsignable,
}

/// Why a listed file cannot be signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsignable {
    /// It is not on disk: listed by an update that could not move it in.
    Missing,
    /// Its entry has no `%SHA256SUM%`, which is what tells that the file
    /// signed is the one listed.
    NoChecksum,
    /// It cannot be read as a package, for this reason.
    NotAPackage { error: String },
    /// It is not the file its entry describes: changed since it was listed.
    NotTheListedFile,
}

impl Unsignable {
    /// What an operator is told about a file left unsigned for this.
    #[must_use]
    pub fn explain(&self) -> String {
        let what = match self {
            Self::Missing => {
                "it is listed but not on disk, so no client can download it either".to_string()
            }
            Self::NoChecksum => {
                "its database entry has no %SHA256SUM% to check the file against".to_string()
            }
            Self::NotAPackage { error } => format!("it cannot be read as a package: {error}"),
            Self::NotTheListedFile => {
                "it is not the file its database entry describes, which clients check too"
                    .to_string()
            }
        };
        format!("{what}; rebuilding its package publishes it again")
    }
}

/// Backfills run one after another, each remembering in memory, for the
/// next, the listed files it found it cannot sign: reported once.
#[derive(Debug, Default)]
pub struct Backfiller {
    /// The listed files the last run found it cannot sign.
    skipped: HashSet<PathBuf>,
}

impl Backfiller {
    /// Sign every package the repository lists that does not carry its
    /// key's signature, then the databases of every platform that do not.
    ///
    /// Packages published meanwhile are signed by their own publication, and
    /// ones retired meanwhile are left alone. Safe to run again at any point:
    /// an interrupted run leaves either the entries it had not committed,
    /// unsigned, or a recovery plan the next update finishes.
    pub async fn run(
        &mut self,
        db: &DatabaseConnection,
        repo: &Repository,
    ) -> anyhow::Result<Backfill> {
        let Some(key) = repo.key() else {
            return Ok(Backfill::Complete {
                signed: 0,
                skipped: Vec::new(),
            });
        };
        let mut signed = 0;
        let mut skipped = Vec::new();
        let mut still_skipped = HashSet::new();
        let mut problems = Vec::new();
        for platform in Platform::ALL {
            let pass = backfill_platform(db, repo, key, platform).await?;
            signed += pass.signed;
            for skip in pass.skipped {
                still_skipped.insert(skip.path.clone());
                if !self.skipped.contains(&skip.path) {
                    skipped.push(skip);
                }
            }
            problems.extend(pass.problems);
            if !repo.databases_signed(platform).await? {
                let mut update = repo.begin(db).await?;
                update.rewrite_databases(platform);
                update.commit(db, |_| Box::pin(async { Ok(()) })).await?;
            }
        }
        // Only a run that returns what it skipped has reported it.
        self.skipped = still_skipped;
        Ok(if problems.is_empty() {
            Backfill::Complete { signed, skipped }
        } else {
            Backfill::Incomplete {
                signed,
                skipped,
                problems,
            }
        })
    }
}

/// How many packages each update signs, of `count` to sign on a platform.
fn batch_size(count: usize) -> usize {
    count.div_ceil(BATCHES).max(MIN_BATCH)
}

/// What a pass over one platform did.
#[derive(Default)]
struct Pass {
    signed: usize,
    skipped: Vec<Skipped>,
    problems: Vec<String>,
}

/// A published file's copy, signed and described, with the entry the
/// database had for it when it was found not signed.
struct Staged {
    path: PathBuf,
    entry: PackageEntry,
    before: ListedPackage,
}

/// What became of staging one listed file.
enum Staging {
    Staged(Staged),
    /// It cannot be signed.
    Unsignable(Unsignable),
}

async fn backfill_platform(
    db: &DatabaseConnection,
    repo: &Repository,
    key: &InstanceKey,
    platform: Platform,
) -> anyhow::Result<Pass> {
    let Unsigned { to_sign, missing } = repo.unsigned(platform).await?;
    let mut pass = Pass {
        skipped: missing
            .into_iter()
            .map(|path| Skipped {
                path,
                why: Unsignable::Missing,
            })
            .collect(),
        ..Pass::default()
    };
    if to_sign.is_empty() {
        return Ok(pass);
    }
    // Whatever an interrupted pass left is only reused once no recovery plan
    // can still name it.
    repo.recover(db).await?;
    discard(repo, platform).await;
    let published = |filename: &str| repo.platform_file(platform, filename);
    for batch in to_sign.chunks(batch_size(to_sign.len())) {
        let mut staged = Vec::with_capacity(batch.len());
        for before in batch {
            match stage(repo, platform, before).await {
                Ok(Staging::Staged(copy)) => staged.push(copy),
                Ok(Staging::Unsignable(why)) => pass.skipped.push(Skipped {
                    path: published(&before.filename),
                    why,
                }),
                Err(e) => pass
                    .problems
                    .push(format!("{platform}/{}: {e:#}", before.filename)),
            }
        }
        let resigned = resign(db, repo, key, platform, staged).await?;
        pass.signed += resigned.signed;
        pass.problems.extend(resigned.problems);
        pass.skipped
            .extend(resigned.unsignable.into_iter().map(|filename| Skipped {
                path: published(&filename),
                why: Unsignable::NotTheListedFile,
            }));
    }
    discard(repo, platform).await;
    Ok(pass)
}

/// Copy, sign and describe the published file `before` lists.
async fn stage(
    repo: &Repository,
    platform: Platform,
    before: &ListedPackage,
) -> anyhow::Result<Staging> {
    if before.sha256sum.is_none() {
        return Ok(Staging::Unsignable(Unsignable::NoChecksum));
    }
    let path = repo.stage_published(platform, &before.filename).await?;
    let entry = match repo.describe(path.clone()).await {
        Ok(entry) => entry,
        Err(error) => {
            return match error.downcast_ref::<NotAPackage>() {
                Some(not) => Ok(Staging::Unsignable(Unsignable::NotAPackage {
                    error: not.to_string(),
                })),
                None => Err(error),
            };
        }
    };
    Ok(Staging::Staged(Staged {
        path,
        entry,
        before: before.clone(),
    }))
}

/// What [`resign`] did with a batch.
#[derive(Default)]
struct Resigned {
    signed: usize,
    problems: Vec<String>,
    /// The files that are not the ones their entries describe.
    unsignable: Vec<String>,
}

/// Replace the entries of the files `staged` copied, in one update, where the
/// repository still lists the same file -- found not signed by the key,
/// before the lock.
async fn resign(
    db: &DatabaseConnection,
    repo: &Repository,
    key: &InstanceKey,
    platform: Platform,
    staged: Vec<Staged>,
) -> anyhow::Result<Resigned> {
    let mut resigned = Resigned::default();
    if staged.is_empty() {
        return Ok(resigned);
    }
    let mut update = repo.begin(db).await?;
    let listed: HashMap<String, ListedPackage> = update
        .listed(platform)
        .await?
        .into_iter()
        .map(|listed| (listed.filename.clone(), listed))
        .collect();
    for Staged {
        path,
        entry,
        before,
    } in staged
    {
        match listed.get(&entry.filename) {
            // Retired since: nothing to sign.
            None => {}
            // Published again since: signed as it was, or left to the next run.
            Some(now) if now.sha256sum != before.sha256sum => {
                if !signed_by(key, now) {
                    resigned.problems.push(format!(
                        "{platform}/{}: not the file that was signed any more",
                        entry.filename
                    ));
                }
            }
            Some(_) if before.sha256sum.as_deref() == Some(entry.sha256sum.as_str()) => {
                update.resign(platform, path, entry);
                resigned.signed += 1;
            }
            Some(_) => resigned.unsignable.push(entry.filename),
        }
    }
    if resigned.signed > 0 {
        update.commit(db, |_| Box::pin(async { Ok(()) })).await?;
    }
    Ok(resigned)
}

/// Whether `listed`'s entry carries a signature `key` made: what an entry
/// the repository published since, through [`Repository::describe`], does.
fn signed_by(key: &InstanceKey, listed: &ListedPackage) -> bool {
    listed
        .signature
        .as_deref()
        .is_some_and(|signature| key.issued(signature))
}

/// Remove what a pass staged. Best-effort: what is left is only ever
/// replaced, never read as it is.
async fn discard(repo: &Repository, platform: Platform) {
    let _ = tokio::fs::remove_dir_all(repo.resign_dir(platform)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A minimal `.pkg.tar.zst` whose bytes `desc` sets apart from another
    /// with the same name and version.
    fn fake_pkg_zst(pkgname: &str, pkgver: &str, desc: &str) -> Vec<u8> {
        let pkginfo = format!("pkgname = {pkgname}\npkgver = {pkgver}\npkgdesc = {desc}\n");
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_path(".PKGINFO").unwrap();
            header.set_size(pkginfo.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, pkginfo.as_bytes()).unwrap();
            builder.finish().unwrap();
        }
        zstd::stream::encode_all(tar_bytes.as_slice(), 0).unwrap()
    }

    fn filename(pkgname: &str) -> String {
        format!("{pkgname}-1.0-1-x86_64.pkg.tar.zst")
    }

    /// Publish `pkgname` 1.0-1 through `repo`, as build `build_id`.
    async fn publish(
        db: &DatabaseConnection,
        repo: &Repository,
        build_id: i32,
        pkgname: &str,
        desc: &str,
    ) {
        let dir = repo.staging_dir(build_id);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(filename(pkgname));
        std::fs::write(&path, fake_pkg_zst(pkgname, "1.0-1", desc)).unwrap();
        let entry = repo.describe(path.clone()).await.unwrap();
        let mut update = repo.begin(db).await.unwrap();
        update.add(Platform::X86_64, path, entry);
        update
            .commit(db, |_| Box::pin(async { Ok(()) }))
            .await
            .unwrap();
    }

    fn platform_file(root: &Path, name: &str) -> PathBuf {
        root.join("x86_64").join(name)
    }

    /// A complete backfill that signed `signed` packages and found nothing
    /// missing.
    fn complete(signed: usize) -> Backfill {
        Backfill::Complete {
            signed,
            skipped: Vec::new(),
        }
    }

    /// Whether `key` signed the file at `path`, beside it, as it is now.
    fn signed_beside(key: &InstanceKey, path: &Path) -> bool {
        let mut signature = path.as_os_str().to_owned();
        signature.push(".sig");
        std::fs::read(signature)
            .is_ok_and(|signature| key.verifies(&signature, &std::fs::read(path).unwrap()))
    }

    /// Turning signing on for a repository with packages in it signs every
    /// one, in its entry and beside it, and the databases; the packages
    /// themselves are untouched, and a second run has nothing to do.
    #[tokio::test]
    async fn a_backfill_signs_what_was_published_unsigned() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let unsigned = Repository::new(tmp.path());
        unsigned.init().unwrap();
        publish(&db, &unsigned, 1, "foo", "one").await;
        publish(&db, &unsigned, 2, "bar", "one").await;
        let foo = platform_file(tmp.path(), &filename("foo"));
        let before = std::fs::read(&foo).unwrap();

        let repo = Repository::new(tmp.path()).with_key(InstanceKey::generate(None).unwrap());
        repo.init().unwrap();
        let mut backfiller = Backfiller::default();
        assert_eq!(backfiller.run(&db, &repo).await.unwrap(), complete(2));

        let key = repo.key().unwrap();
        let db_archive = platform_file(tmp.path(), "repo.db.tar.gz");
        for listed in pacman_repo_utils::listed_packages(&db_archive).unwrap() {
            let path = platform_file(tmp.path(), &listed.filename);
            let signature = listed.signature.expect("signed in its entry");
            assert!(key.verifies(&signature, &std::fs::read(&path).unwrap()));
            assert!(signed_beside(key, &path));
        }
        assert_eq!(std::fs::read(&foo).unwrap(), before);
        assert!(signed_beside(key, &db_archive));
        assert!(signed_beside(
            key,
            &platform_file(tmp.path(), "repo.files.tar.gz")
        ));
        assert!(!repo.resign_dir(Platform::X86_64).exists());

        assert_eq!(backfiller.run(&db, &repo).await.unwrap(), complete(0));
        assert_eq!(
            Backfiller::default().run(&db, &repo).await.unwrap(),
            complete(0)
        );
    }

    /// Packages signed by another key are signed again with this one.
    #[tokio::test]
    async fn a_backfill_replaces_another_keys_signatures() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let old = Repository::new(tmp.path()).with_key(InstanceKey::generate(None).unwrap());
        publish(&db, &old, 1, "foo", "one").await;

        let repo = Repository::new(tmp.path()).with_key(InstanceKey::generate(None).unwrap());
        assert_eq!(
            Backfiller::default().run(&db, &repo).await.unwrap(),
            complete(1)
        );
        let key = repo.key().unwrap();
        assert!(signed_beside(
            key,
            &platform_file(tmp.path(), &filename("foo"))
        ));
        assert!(signed_beside(
            key,
            &platform_file(tmp.path(), "repo.db.tar.gz")
        ));
    }

    /// A file published again under the same name after its copy was signed
    /// is not given the copy's entry: that would list the old file's checksum
    /// for the new one.
    #[tokio::test]
    async fn a_file_replaced_after_it_was_copied_keeps_its_own_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let unsigned = Repository::new(tmp.path());
        publish(&db, &unsigned, 1, "foo", "one").await;

        let repo = Repository::new(tmp.path()).with_key(InstanceKey::generate(None).unwrap());
        let key = repo.key().unwrap();
        let listed = repo.listed(Platform::X86_64).await.unwrap();
        let Staging::Staged(copy) = stage(&repo, Platform::X86_64, &listed[0]).await.unwrap()
        else {
            panic!("the file is a package");
        };
        publish(&db, &unsigned, 2, "foo", "two").await;
        let db_before = std::fs::read(platform_file(tmp.path(), "repo.db.tar.gz")).unwrap();

        let resigned = resign(&db, &repo, key, Platform::X86_64, vec![copy])
            .await
            .unwrap();
        assert_eq!(resigned.signed, 0);
        assert_eq!(resigned.problems.len(), 1, "{:?}", resigned.problems);
        assert!(resigned.unsignable.is_empty());
        assert_eq!(
            std::fs::read(platform_file(tmp.path(), "repo.db.tar.gz")).unwrap(),
            db_before
        );
    }

    /// A listed file that is not a package any more, or not the one its
    /// entry describes -- changed behind the repository's back -- is
    /// reported by the first run that finds it, and by no later one, and
    /// does not keep the backfill from completing: signing it again would
    /// fail the same way.
    #[tokio::test]
    async fn a_file_that_cannot_be_signed_is_reported_once() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let unsigned = Repository::new(tmp.path());
        unsigned.init().unwrap();
        publish(&db, &unsigned, 1, "foo", "one").await;
        publish(&db, &unsigned, 2, "bar", "one").await;
        publish(&db, &unsigned, 3, "baz", "one").await;
        let broken = platform_file(tmp.path(), &filename("foo"));
        std::fs::write(&broken, b"not a package").unwrap();
        let changed = platform_file(tmp.path(), &filename("bar"));
        std::fs::write(&changed, fake_pkg_zst("bar", "1.0-1", "changed")).unwrap();

        let repo = Repository::new(tmp.path()).with_key(InstanceKey::generate(None).unwrap());
        repo.init().unwrap();
        let mut backfiller = Backfiller::default();
        let Backfill::Complete { signed, skipped } = backfiller.run(&db, &repo).await.unwrap()
        else {
            panic!("what cannot be signed does not keep the backfill going");
        };
        assert_eq!(signed, 1);
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(
            skipped
                .iter()
                .any(|skip| skip.path == broken
                    && matches!(skip.why, Unsignable::NotAPackage { .. }))
        );
        assert!(skipped.contains(&Skipped {
            path: changed.clone(),
            why: Unsignable::NotTheListedFile,
        }));
        let key = repo.key().unwrap();
        assert!(signed_beside(
            key,
            &platform_file(tmp.path(), &filename("baz"))
        ));
        assert!(signed_beside(
            key,
            &platform_file(tmp.path(), "repo.db.tar.gz")
        ));
        assert!(
            skipped
                .iter()
                .all(|skip| skip.why.explain().contains("rebuilding"))
        );

        assert_eq!(backfiller.run(&db, &repo).await.unwrap(), complete(0));
    }

    /// A repository whose packages are all signed, but not its databases --
    /// none were ever written by an update since it signs -- has its
    /// databases written again, and signed, once.
    #[tokio::test]
    async fn a_backfill_signs_databases_nothing_else_would() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        Repository::new(tmp.path()).init().unwrap();

        let repo = Repository::new(tmp.path()).with_key(InstanceKey::generate(None).unwrap());
        repo.init().unwrap();
        assert_eq!(
            Backfiller::default().run(&db, &repo).await.unwrap(),
            complete(0)
        );
        let key = repo.key().unwrap();
        for platform in Platform::ALL {
            assert!(repo.databases_signed(platform).await.unwrap(), "{platform}");
        }
        assert!(signed_beside(
            key,
            &platform_file(tmp.path(), "repo.files.tar.gz")
        ));
        let signature = std::fs::read(platform_file(tmp.path(), "repo.db.tar.gz.sig")).unwrap();
        assert_eq!(
            Backfiller::default().run(&db, &repo).await.unwrap(),
            complete(0)
        );
        assert_eq!(
            std::fs::read(platform_file(tmp.path(), "repo.db.tar.gz.sig")).unwrap(),
            signature,
            "signed databases are not written again"
        );
    }

    /// An update signs more than one batch's worth at a time in a larger
    /// repository, so the databases are written a bounded number of times.
    #[test]
    fn a_batch_grows_with_the_repository() {
        assert_eq!(batch_size(1), MIN_BATCH);
        assert_eq!(batch_size(MIN_BATCH * BATCHES), MIN_BATCH);
        assert_eq!(batch_size(10_000), 1_250);
        assert!(10_000_usize.div_ceil(batch_size(10_000)) <= BATCHES);
    }

    /// A file the database lists that is not on disk -- what an update whose
    /// staged file was gone before it could be moved in leaves -- is
    /// reported by the first run that finds it, and by no later one, and
    /// does not keep the backfill from completing: nothing can sign it.
    #[tokio::test]
    async fn a_listed_file_missing_from_disk_is_reported_once() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let unsigned = Repository::new(tmp.path());
        unsigned.init().unwrap();
        publish(&db, &unsigned, 1, "foo", "one").await;
        publish(&db, &unsigned, 2, "bar", "one").await;
        let gone = platform_file(tmp.path(), &filename("foo"));
        std::fs::remove_file(&gone).unwrap();

        let repo = Repository::new(tmp.path()).with_key(InstanceKey::generate(None).unwrap());
        repo.init().unwrap();
        let mut backfiller = Backfiller::default();
        assert_eq!(
            backfiller.run(&db, &repo).await.unwrap(),
            Backfill::Complete {
                signed: 1,
                skipped: vec![Skipped {
                    path: gone.clone(),
                    why: Unsignable::Missing,
                }],
            }
        );
        let key = repo.key().unwrap();
        assert!(signed_beside(
            key,
            &platform_file(tmp.path(), &filename("bar"))
        ));
        assert!(!gone.exists());
        assert!(signed_beside(
            key,
            &platform_file(tmp.path(), "repo.db.tar.gz")
        ));

        assert_eq!(backfiller.run(&db, &repo).await.unwrap(), complete(0));
    }

    /// A repository that does not sign has nothing to backfill.
    #[tokio::test]
    async fn an_unsigned_repository_has_nothing_to_backfill() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let repo = Repository::new(tmp.path());
        publish(&db, &repo, 1, "foo", "one").await;
        assert_eq!(
            Backfiller::default().run(&db, &repo).await.unwrap(),
            complete(0)
        );
        let db_archive = platform_file(tmp.path(), "repo.db.tar.gz");
        assert!(
            pacman_repo_utils::listed_packages(&db_archive).unwrap()[0]
                .signature
                .is_none()
        );
    }
}
