//! Bringing what the repository already publishes under its key.
//!
//! A repository that signs signs every package it publishes from then on
//! ([`Repository::describe`]). What it published before -- unsigned, or
//! signed by a key it no longer uses -- is signed here, through the same
//! steps as publishing: a copy of each file is signed and described before
//! the lock, and an update replaces its entry and moves its signature in. The
//! package files themselves never move, and the databases are signed by the
//! update that rewrites them.

use crate::repository::Repository;
use aurcache_signing::InstanceKey;
use pacman_mirrors::platforms::Platform;
use pacman_repo_utils::{ListedPackage, PackageEntry};
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::path::PathBuf;

/// How many packages one update signs at most. Bounds how long the lock is
/// held at a time, and how much a filesystem without hard links copies at
/// once.
const BATCH: usize = 64;

/// What a backfill did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backfill {
    /// Every listed package and every database carries the key's signature.
    Complete { signed: usize },
    /// Some packages could not be signed this time, each with why.
    Incomplete {
        signed: usize,
        problems: Vec<String>,
    },
}

/// Sign every package the repository lists that does not carry its key's
/// signature, then any database that does not.
///
/// Packages published meanwhile are signed by their own publication, and
/// ones retired meanwhile are left alone. Safe to run again at any point: an
/// interrupted run leaves either the entries it had not committed, unsigned,
/// or a recovery plan the next update finishes.
pub async fn backfill(db: &DatabaseConnection, repo: &Repository) -> anyhow::Result<Backfill> {
    let Some(key) = repo.key() else {
        return Ok(Backfill::Complete { signed: 0 });
    };
    let mut signed = 0;
    let mut problems = Vec::new();
    for platform in Platform::ALL {
        let pass = backfill_platform(db, repo, key, platform).await?;
        signed += pass.signed;
        problems.extend(pass.problems);
    }
    repo.sign_databases(db).await?;
    Ok(if problems.is_empty() {
        Backfill::Complete { signed }
    } else {
        Backfill::Incomplete { signed, problems }
    })
}

/// What a pass over one platform did.
#[derive(Default)]
struct Pass {
    signed: usize,
    problems: Vec<String>,
}

/// A published file's copy, signed and described, with the entry the
/// database had for it when it was found not signed.
struct Staged {
    path: PathBuf,
    entry: PackageEntry,
    before: ListedPackage,
}

async fn backfill_platform(
    db: &DatabaseConnection,
    repo: &Repository,
    key: &InstanceKey,
    platform: Platform,
) -> anyhow::Result<Pass> {
    let unsigned = repo.unsigned(platform).await?;
    let mut pass = Pass::default();
    if unsigned.is_empty() {
        return Ok(pass);
    }
    // Whatever an interrupted pass left is only reused once no recovery plan
    // can still name it.
    repo.recover(db).await?;
    discard(repo, platform).await;
    for batch in unsigned.chunks(BATCH) {
        let mut staged = Vec::with_capacity(batch.len());
        for before in batch {
            match stage(repo, platform, before).await {
                Ok(copy) => staged.push(copy),
                Err(e) => pass
                    .problems
                    .push(format!("{platform}/{}: {e:#}", before.filename)),
            }
        }
        pass.signed += resign(db, repo, key, platform, staged, &mut pass.problems).await?;
    }
    discard(repo, platform).await;
    Ok(pass)
}

/// Copy, sign and describe the published file `before` lists.
async fn stage(
    repo: &Repository,
    platform: Platform,
    before: &ListedPackage,
) -> anyhow::Result<Staged> {
    let path = repo.stage_published(platform, &before.filename).await?;
    let entry = repo.describe(path.clone()).await?;
    Ok(Staged {
        path,
        entry,
        before: before.clone(),
    })
}

/// Replace the entries of the files `staged` copied, in one update, where the
/// repository still lists the same file -- found not signed by the key over
/// what it is, before the lock. Returns how many it did.
async fn resign(
    db: &DatabaseConnection,
    repo: &Repository,
    key: &InstanceKey,
    platform: Platform,
    staged: Vec<Staged>,
    problems: &mut Vec<String>,
) -> anyhow::Result<usize> {
    if staged.is_empty() {
        return Ok(0);
    }
    let mut update = repo.begin(db).await?;
    let listed: HashMap<String, ListedPackage> = update
        .listed(platform)
        .await?
        .into_iter()
        .map(|listed| (listed.filename.clone(), listed))
        .collect();
    let mut count = 0;
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
                    problems.push(format!(
                        "{platform}/{}: not the file that was signed any more",
                        entry.filename
                    ));
                }
            }
            Some(_) if before.sha256sum.as_deref() == Some(entry.sha256sum.as_str()) => {
                update.resign(platform, path, entry);
                count += 1;
            }
            Some(_) => problems.push(format!(
                "{platform}/{}: the file is not the one its entry describes",
                entry.filename
            )),
        }
    }
    if count > 0 {
        update.commit(db, |_| Box::pin(async { Ok(()) })).await?;
    }
    Ok(count)
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
        assert_eq!(
            backfill(&db, &repo).await.unwrap(),
            Backfill::Complete { signed: 2 }
        );

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

        assert_eq!(
            backfill(&db, &repo).await.unwrap(),
            Backfill::Complete { signed: 0 }
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
            backfill(&db, &repo).await.unwrap(),
            Backfill::Complete { signed: 1 }
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
        let copy = stage(&repo, Platform::X86_64, &listed[0]).await.unwrap();
        publish(&db, &unsigned, 2, "foo", "two").await;
        let db_before = std::fs::read(platform_file(tmp.path(), "repo.db.tar.gz")).unwrap();

        let mut problems = Vec::new();
        let signed = resign(&db, &repo, key, Platform::X86_64, vec![copy], &mut problems)
            .await
            .unwrap();
        assert_eq!(signed, 0);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(
            std::fs::read(platform_file(tmp.path(), "repo.db.tar.gz")).unwrap(),
            db_before
        );
    }

    /// An entry whose `%PGPSIG%` names the key but is not over its file --
    /// what an issuer check alone would take for signed -- is signed again,
    /// and so is the file beside it.
    #[tokio::test]
    async fn a_signature_that_only_names_the_key_is_signed_again() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let repo = Repository::new(tmp.path()).with_key(InstanceKey::generate(None).unwrap());
        repo.init().unwrap();
        let key = repo.key().unwrap();
        let dir = repo.staging_dir(1);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(filename("foo"));
        std::fs::write(&path, fake_pkg_zst("foo", "1.0-1", "one")).unwrap();
        let mut beside = path.as_os_str().to_owned();
        beside.push(".sig");
        std::fs::write(&beside, key.sign(&b"something else"[..]).unwrap()).unwrap();
        let entry = pacman_repo_utils::describe_package(&path).unwrap();
        let mut update = repo.begin(&db).await.unwrap();
        update.add(Platform::X86_64, path, entry);
        update
            .commit(&db, |_| Box::pin(async { Ok(()) }))
            .await
            .unwrap();
        let published = platform_file(tmp.path(), &filename("foo"));
        let listed = repo.listed(Platform::X86_64).await.unwrap();
        assert!(key.issued(listed[0].signature.as_deref().unwrap()));
        assert_eq!(repo.unsigned(Platform::X86_64).await.unwrap().len(), 1);

        assert_eq!(
            backfill(&db, &repo).await.unwrap(),
            Backfill::Complete { signed: 1 }
        );
        let listed = repo.listed(Platform::X86_64).await.unwrap();
        assert!(key.verifies_file(listed[0].signature.as_deref().unwrap(), &published));
        assert!(signed_beside(key, &published));
        assert!(repo.unsigned(Platform::X86_64).await.unwrap().is_empty());
    }

    /// A repository that does not sign has nothing to backfill.
    #[tokio::test]
    async fn an_unsigned_repository_has_nothing_to_backfill() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let repo = Repository::new(tmp.path());
        publish(&db, &repo, 1, "foo", "one").await;
        assert_eq!(
            backfill(&db, &repo).await.unwrap(),
            Backfill::Complete { signed: 0 }
        );
        let db_archive = platform_file(tmp.path(), "repo.db.tar.gz");
        assert!(
            pacman_repo_utils::listed_packages(&db_archive).unwrap()[0]
                .signature
                .is_none()
        );
    }
}
