//! What a worker is handed when it claims a build.

use crate::job_config::{JobConfig, build_job_config, mirrorlist_dir, mirrorlist_for};
use crate::settings;
use crate::snapshot::SnapshotStore;
use crate::vcs_check::job_vcs_sources;
use aurcache_common::digest::sha256_hex;
use aurcache_common::worker::{JobDescriptor, MirrorlistOffer, MirrorlistPreference};
use aurcache_db::builds;
use aurcache_db::prelude::Packages;
use sea_orm::{DatabaseConnection, EntityTrait};

/// The job descriptor for `build`, for a worker holding `held_mirrorlist`.
pub async fn descriptor(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    build: &builds::Model,
    held_mirrorlist: &MirrorlistPreference,
) -> anyhow::Result<JobDescriptor> {
    let pkg = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("package {} not found", build.pkg_id))?;

    let arch = build.platform.as_str().to_string();
    let JobConfig {
        makepkg_conf,
        pacman_conf,
    } =
        // No `[repo]` section here: the worker appends one rendered from the
        // template it received at registration, using the host it actually
        // reaches this server on.
        build_job_config(db, pkg.id, &arch).await?;
    let mirrorlist = resolve_mirrorlist(
        held_mirrorlist,
        &arch,
        mirrorlist_for(&arch, &mirrorlist_dir()).await,
    );

    // Best-effort, from the parsed .SRCINFO: the `validpgpkeys` the worker must
    // trust, and the `git+` sources whose commit it should report back. A
    // parse failure costs neither outright -- the worker can still self-extract
    // the keys, and an unreported source leaves the commit recorded at queue
    // time standing -- as does a PKGBUILD that cannot be read, which reports
    // no source at all.
    let (pgp_keys, vcs_sources) = match store
        .sourceinfo(&pkg.source_data, pkg.patch.as_deref())
        .await
    {
        Ok(si) => {
            let pkgbuild = store
                .pkgbuild(&pkg.source_data, pkg.patch.as_deref())
                .await
                .unwrap_or_default();
            (
                si.base
                    .pgp_fingerprints
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                job_vcs_sources(&si, &pkgbuild),
            )
        }
        Err(_) => (Vec::new(), Vec::new()),
    };

    // With the flags every build takes in front. Workers on protocol 1 rely on
    // it: the protocol has makepkg given `build_flags` as they are, so a
    // worker that adds none of these itself builds with exactly this list.
    let build_flags = aurcache_common::build_flags::for_makepkg(pkg.build_flags.as_slice());

    // Resolved here rather than on the worker: settings are the server's,
    // with the package overriding the global default.
    let persistent_builddir =
        settings::get(db, settings::key::PERSISTENT_BUILDDIR, Some(build.pkg_id))
            .await?
            .value;

    // The package's job timeout, for the worker to stop the build at. The
    // lease reaper holds the build to the same limit, plus a grace, only as a
    // backstop for a worker that cannot.
    let timeout = settings::get(db, settings::key::JOB_TIMEOUT, Some(build.pkg_id))
        .await?
        .value
        .0;

    let ssh_key = ssh_key(db, build.pkg_id).await?;

    // The package's local hosts added to the server's. Sent with every job,
    // `[]` when there are none: only a server that predates the field leaves
    // it out.
    let build_hosts = settings::build_hosts(db, Some(build.pkg_id))
        .await?
        .union()
        .iter()
        .map(ToString::to_string)
        .collect();

    // Before `pkg.name` moves into the descriptor below.
    let packages = crate::publish::expected_pkgnames(&pkg);
    Ok(JobDescriptor {
        build_id: build.id,
        pkgbase: pkg.name,
        expected_packages: packages,
        expected_version: build.version.clone(),
        timeout_secs: Some(timeout),
        arch,
        build_flags,
        persistent_builddir,
        makepkg_conf,
        pacman_conf,
        mirrorlist,
        pgp_keys,
        vcs_sources,
        ssh_key,
        build_hosts: Some(build_hosts),
    })
}

/// The worker-held SSH key `pkg_id`'s builds may use, by name: its
/// `build_ssh_key` setting.
///
/// Left out for the default key, which is what a job without the field
/// asks for: a worker holding no key by that name then builds without one
/// quietly, as it does for every job of a server that names no keys. Any
/// other name is sent, and a worker that does not hold it says so in the
/// build log.
async fn ssh_key(db: &DatabaseConnection, pkg_id: i32) -> anyhow::Result<Option<String>> {
    let name = settings::get(db, settings::key::BUILD_SSH_KEY, Some(pkg_id))
        .await?
        .value
        .0;
    Ok((name != aurcache_common::ssh_key::DEFAULT).then_some(name))
}

/// Decide what to put in a job descriptor for the mirrorlist.
///
/// The mirrorlist is deployment configuration rather than build configuration,
/// but it is delivered on the claim because it *changes* --
/// `MIRROR_RANK_SCHEDULE` rewrites it on a schedule -- and the claim is the
/// only exchange that recurs. Revalidating a checksum keeps that from meaning
/// the same bytes with every job.
///
/// The checksum is computed only here. A worker never digests anything -- it
/// echoes back the value it was sent -- so this can change algorithm without
/// any worker needing to agree, at the cost of one resend per worker.
fn resolve_mirrorlist(
    held: &MirrorlistPreference,
    arch: &str,
    current: Option<String>,
) -> MirrorlistOffer {
    // A worker with its own mirrorlist would discard anything sent, so nothing
    // is computed or sent for it.
    let MirrorlistPreference::Server { checksums } = held else {
        return MirrorlistOffer::Nothing;
    };
    let Some(content) = current else {
        return MirrorlistOffer::Nothing;
    };
    let checksum = sha256_hex(&content);
    if checksums.get(arch).map(String::as_str) == Some(checksum.as_str()) {
        MirrorlistOffer::Keep
    } else {
        MirrorlistOffer::Send { content, checksum }
    }
}

#[cfg(test)]
mod tests {
    use super::{descriptor, resolve_mirrorlist, ssh_key};
    use crate::settings::{Change, write};
    use crate::snapshot::SnapshotStore;
    use aurcache_common::settings::Setting;
    use aurcache_common::worker::{MirrorlistOffer, MirrorlistPreference};
    use aurcache_db::testing::{build, memory_db, package};
    use sea_orm::ActiveValue::Set;
    use std::collections::BTreeMap;

    /// The `build_flags` of the descriptor for a build of a package storing
    /// `stored`, written as the column is.
    async fn descriptor_flags(stored: &str) -> Vec<String> {
        let db = memory_db().await;
        let pkg = package("hello")
            .with(|row| row.build_flags = Set(stored.parse().expect("build flags")))
            .insert(&db)
            .await;
        let build = build(&pkg).insert(&db).await;
        // No source to fetch, which a descriptor does without.
        let aur = tempfile::tempdir().unwrap();
        let checkouts = tempfile::tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root_and_aur_base(
            checkouts.path().to_path_buf(),
            aur.path().to_string_lossy().to_string(),
        );
        descriptor(&db, &store, &build, &MirrorlistPreference::Local)
            .await
            .expect("a descriptor")
            .build_flags
    }

    /// A worker on protocol 1 may add no flag of its own, so the descriptor
    /// carries the ones every build takes, then the package's.
    #[tokio::test]
    async fn the_descriptor_carries_the_flags_every_build_takes() {
        assert_eq!(
            descriptor_flags("").await,
            ["--noconfirm", "--noprogressbar", "--nocolor"]
        );
        assert_eq!(
            descriptor_flags("--nocheck").await,
            ["--noconfirm", "--noprogressbar", "--nocolor", "--nocheck"]
        );
    }

    /// The job names the key the package's setting names, and leaves the
    /// field out for the default key.
    #[tokio::test]
    async fn the_job_carries_the_packages_ssh_key_setting() {
        let db = memory_db().await;

        assert_eq!(ssh_key(&db, 1).await.unwrap(), None);

        write(
            &db,
            [Change {
                setting: Setting::BuildSshKey,
                pkg_id: Some(1),
                value: Some("gitlab".to_string()),
            }],
        )
        .await
        .unwrap();
        assert_eq!(ssh_key(&db, 1).await.unwrap(), Some("gitlab".to_string()));
        assert_eq!(ssh_key(&db, 2).await.unwrap(), None);

        // Named outright, the default is still left out.
        write(
            &db,
            [Change {
                setting: Setting::BuildSshKey,
                pkg_id: Some(1),
                value: Some("default".to_string()),
            }],
        )
        .await
        .unwrap();
        assert_eq!(ssh_key(&db, 1).await.unwrap(), None);
    }

    /// Every job carries `build_hosts`: `[]` on the wire when neither the
    /// package nor the server names a host, and otherwise the package's
    /// followed by the server's.
    #[tokio::test]
    async fn every_job_carries_its_build_hosts() {
        let db = aurcache_db::testing::memory_db().await;
        let pkg = aurcache_db::testing::package("hello").insert(&db).await;
        let build = aurcache_db::testing::build(&pkg).insert(&db).await;
        // Sources nothing can be fetched from: the job's keys and VCS sources
        // come out empty, which is all this needs.
        let checkouts = tempfile::tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root_and_aur_base(
            checkouts.path().to_path_buf(),
            checkouts
                .path()
                .join("no-aur")
                .to_string_lossy()
                .to_string(),
        );
        let job = || descriptor(&db, &store, &build, &MirrorlistPreference::Local);

        let wire = serde_json::to_value(job().await.unwrap()).unwrap();
        assert_eq!(wire["build_hosts"], serde_json::json!([]));

        write(
            &db,
            [
                Change {
                    setting: Setting::BuildHosts,
                    pkg_id: None,
                    value: Some("distcc.lan:3632".to_string()),
                },
                Change {
                    setting: Setting::BuildHosts,
                    pkg_id: Some(pkg.id),
                    value: Some("gitlab.lan:22".to_string()),
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            job().await.unwrap().build_hosts,
            Some(vec![
                "gitlab.lan:22".to_string(),
                "distcc.lan:3632".to_string()
            ])
        );
    }

    fn holding(arch: &str, checksum: &str) -> MirrorlistPreference {
        let mut checksums = BTreeMap::new();
        checksums.insert(arch.to_string(), checksum.to_string());
        MirrorlistPreference::Server { checksums }
    }

    fn holding_nothing() -> MirrorlistPreference {
        MirrorlistPreference::Server {
            checksums: BTreeMap::new(),
        }
    }

    /// The checksum a worker sent `list` would hold.
    fn checksum_sent_with(list: &str) -> String {
        match resolve_mirrorlist(&holding_nothing(), "x86_64", Some(list.to_string())) {
            MirrorlistOffer::Send { checksum, .. } => checksum,
            other => panic!("expected the list to be sent, got {other:?}"),
        }
    }

    /// A worker that holds nothing is sent the content, with the checksum it
    /// should echo back next time.
    #[test]
    fn a_worker_holding_nothing_is_sent_the_mirrorlist() {
        let offer = resolve_mirrorlist(
            &holding_nothing(),
            "x86_64",
            Some("Server = http://mirror/\n".to_string()),
        );
        assert!(
            matches!(&offer, MirrorlistOffer::Send { content, .. } if content == "Server = http://mirror/\n"),
            "{offer:?}"
        );
    }

    /// The point of the whole exchange: matching checksum, no bytes resent.
    #[test]
    fn a_matching_checksum_withholds_the_content() {
        let list = "Server = http://mirror/\n".to_string();
        let checksum = checksum_sent_with(&list);
        let offer = resolve_mirrorlist(&holding("x86_64", &checksum), "x86_64", Some(list));
        assert_eq!(offer, MirrorlistOffer::Keep);
    }

    /// A checksum held for one architecture says nothing about another, which
    /// is why the claim carries a map rather than a single value.
    #[test]
    fn a_checksum_for_another_arch_does_not_match() {
        let list = "Server = http://mirror/\n".to_string();
        let checksum = checksum_sent_with(&list);
        let offer = resolve_mirrorlist(&holding("x86_64", &checksum), "aarch64", Some(list));
        assert!(
            matches!(offer, MirrorlistOffer::Send { .. }),
            "aarch64 has not been sent this list"
        );
    }

    /// "The server has none" must reach the worker as something other than
    /// "keep what you have": it has to drop a stale entry rather than reuse it.
    #[test]
    fn no_server_mirrorlist_is_not_reported_as_unchanged() {
        let offer = resolve_mirrorlist(&holding("x86_64", "whatever"), "x86_64", None);
        assert_eq!(offer, MirrorlistOffer::Nothing);
    }

    /// A worker with its own mirrorlist is sent nothing at all.
    #[test]
    fn a_local_worker_is_sent_nothing() {
        let offer = resolve_mirrorlist(
            &MirrorlistPreference::Local,
            "x86_64",
            Some("Server = http://mirror/\n".to_string()),
        );
        assert_eq!(offer, MirrorlistOffer::Nothing);
    }
}
