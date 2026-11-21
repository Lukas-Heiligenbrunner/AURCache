//! A signed repository for `scripts/test-signed-repo.sh`, which checks it with
//! pacman in a container: the only judge of whether a client accepts it.
//!
//! Ignored, since all it does is write the repository to
//! `AURCACHE_SIGNED_REPO_DIR`. It goes the way a real instance does: one
//! package published unsigned, signing turned on and that package signed by
//! the backfill, then a second package published signed.

use aurcache_signing::{InstanceKey, KEY_FILE};
use aurcache_utils::repository::Repository;
use aurcache_utils::signing::{Backfill, Backfiller};
use pacman_mirrors::platforms::Platform;
use sea_orm::DatabaseConnection;
use std::path::{Path, PathBuf};

/// A package pacman installs: a `.PKGINFO` for any architecture and one file.
fn package(pkgname: &str) -> Vec<u8> {
    let readme = format!("{pkgname} came from a signed repository\n");
    let pkginfo = format!(
        "pkgname = {pkgname}\npkgbase = {pkgname}\npkgver = 1.0-1\npkgdesc = signing check\n\
         url = https://example.com\nbuilddate = 1700000000\npackager = AURCache tests\n\
         size = {}\narch = any\nlicense = MIT\n",
        readme.len()
    );
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        for (path, content) in [
            (".PKGINFO".to_string(), pkginfo.as_bytes()),
            (format!("usr/share/doc/{pkgname}/README"), readme.as_bytes()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_path(&path).unwrap();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(1_700_000_000);
            header.set_cksum();
            builder.append(&header, content).unwrap();
        }
        builder.finish().unwrap();
    }
    zstd::stream::encode_all(tar_bytes.as_slice(), 0).unwrap()
}

async fn publish(db: &DatabaseConnection, repo: &Repository, build_id: i32, pkgname: &str) {
    let dir = repo.staging_dir(build_id);
    std::fs::create_dir_all(&dir).unwrap();
    let staged = dir.join(format!("{pkgname}-1.0-1-any.pkg.tar.zst"));
    std::fs::write(&staged, package(pkgname)).unwrap();
    let entry = repo.describe(staged.clone()).await.unwrap();
    let mut update = repo.begin(db).await.unwrap();
    update.add(Platform::X86_64, staged, entry);
    update
        .commit(db, |_| Box::pin(async { Ok(()) }))
        .await
        .unwrap();
    repo.discard_staging(build_id).await;
}

/// The repository's name, which `scripts/test-signed-repo.sh` configures
/// pacman with.
const NAME: &str = "signed_check";

#[tokio::test]
#[ignore = "writes a repository for scripts/test-signed-repo.sh"]
async fn write_a_signed_repository() {
    let out = PathBuf::from(
        std::env::var_os("AURCACHE_SIGNED_REPO_DIR")
            .expect("AURCACHE_SIGNED_REPO_DIR names where to write the repository"),
    );
    let db = aurcache_db::testing::memory_db().await;
    let root = out.join("repo");

    // A name of its own, so that pacman checks the signatures under it.
    let unsigned = Repository::new(&root).with_name(NAME.into()).unwrap();
    unsigned.init().unwrap();
    publish(&db, &unsigned, 1, "signed-later").await;

    let key = InstanceKey::load_or_create(&out.join("ca").join(KEY_FILE), None).unwrap();
    std::fs::write(out.join("key.asc"), key.public_key()).unwrap();
    std::fs::write(out.join("fingerprint"), key.fingerprint()).unwrap();
    let repo = Repository::new(&root)
        .with_name(NAME.into())
        .unwrap()
        .with_key(key);
    repo.init().unwrap();
    assert_eq!(
        Backfiller::default().run(&db, &repo).await.unwrap(),
        Backfill::Complete {
            signed: 1,
            skipped: Vec::new(),
        }
    );
    publish(&db, &repo, 2, "signed-now").await;

    for name in [
        "signed-later-1.0-1-any.pkg.tar.zst.sig".to_string(),
        "signed-now-1.0-1-any.pkg.tar.zst.sig".to_string(),
        format!("{NAME}.db.sig"),
        format!("{NAME}.files.sig"),
    ] {
        assert!(
            Path::new(&root).join("x86_64").join(&name).is_file(),
            "{name}"
        );
    }
}
