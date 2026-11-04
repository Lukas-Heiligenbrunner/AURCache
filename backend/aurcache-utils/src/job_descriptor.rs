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

    let build_flags = pkg.build_flags.as_slice().to_vec();

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
    })
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
    use super::resolve_mirrorlist;
    use aurcache_common::worker::{MirrorlistOffer, MirrorlistPreference};
    use std::collections::BTreeMap;

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
