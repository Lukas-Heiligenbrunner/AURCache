//! Downloading a lite export of the instance's authored state.

use crate::auth::Authenticated;
use crate::init::{CaDirectory, ServerVersion};
use crate::utils::error::{ApiError, err};
use crate::utils::operation::{self, Counts, Recorder};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::api::dump::{
    ExistingPackagePolicy, RestoreAccepted, RestoreEntry, RestoreOptions, RestoreOutcome,
    RestoreProgress, SecretsPolicy,
};
use aurcache_common::api::operations::OperationProgress;
use aurcache_db::helpers::operations;
use aurcache_utils::services::Services;
use rocket::FromForm;
use rocket::data::Data;
use rocket::http::{Header, Status};
use rocket::response::status;
use rocket::serde::json::Json;
use rocket::{Responder, State, get, post};
use sea_orm::DatabaseConnection;
use std::collections::HashSet;
use tracing::warn;
use utoipa::OpenApi;

/// The largest dump the server will read.
///
/// A lite dump of a very large instance is still small -- text, compressed --
/// so this is far above anything legitimate and exists to stop an unbounded
/// read rather than to constrain real use.
const MAX_DUMP_SIZE: rocket::data::ByteUnit = rocket::data::ByteUnit::Mebibyte(64);

#[derive(OpenApi)]
#[openapi(paths(dump, restore, restore_progress))]
pub struct DumpApi;

/// The archive, with the filename a browser should save it under.
#[derive(Responder)]
#[response(content_type = "application/gzip")]
pub struct DumpArchive {
    body: Vec<u8>,
    disposition: Header<'static>,
}

/// A filename a browser will save sensibly, and that sorts by date in a
/// directory of backups.
fn dump_file_name(created_at: i64) -> String {
    let stamp = jiff::Timestamp::from_second(created_at).map_or_else(
        |_| created_at.to_string(),
        |t| t.strftime("%Y%m%d").to_string(),
    );
    format!("aurcache-dump-{stamp}.tar.gz")
}

#[utoipa::path(
    responses(
            (status = 200, description = "A .tar.gz of the instance's authored state"),
    )
)]
/// Export packages, settings and approved workers as a `.tar.gz`.
///
/// Authored state only -- build history, logs and the resolved dependency graph
/// are rebuilt by the next resolve and are not carried. No secrets: the CA and
/// worker certificates are deliberately absent, so this is safe to keep
/// alongside ordinary backups.
#[get("/dump?<include_secrets>")]
pub async fn dump(
    services: &State<Services>,
    version: &State<ServerVersion>,
    ca_dir: &State<CaDirectory>,
    include_secrets: Option<bool>,
    a: Authenticated,
) -> Result<DumpArchive, ApiError> {
    let with_secrets = include_secrets.unwrap_or(false);
    if with_secrets {
        // Logged loudly and by name. The CA private key signs worker
        // identities, so whoever takes a copy can mint a worker this server
        // accepts -- for as long as the CA lives, and regardless of whether
        // their own access is revoked later. That is worth a line in the log
        // even when it is entirely legitimate.
        warn!(
            "Exporting a dump WITH SECRETS (CA private key, worker certificates, \
             token hashes), requested by {}",
            a.username.as_deref().unwrap_or("an unauthenticated caller")
        );
    }
    let dump = aurcache_utils::dump::build_dump(
        &services.db,
        &version.0,
        with_secrets.then(|| ca_dir.0.as_path()),
    )
    .await?;
    services.activity.emit_by(
        Event::DumpExported {
            secrets: with_secrets,
        },
        a.username.clone(),
    );
    let created_at = dump.manifest.created_at;
    let bytes = aurcache_utils::dump::write_archive(&dump)?;

    Ok(DumpArchive {
        body: bytes,
        disposition: Header::new(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", dump_file_name(created_at)),
        ),
    })
}

/// A policy from the query string, its default when absent.
fn parse_policy<P>(value: Option<&str>) -> Result<P, ApiError>
where
    P: std::str::FromStr<Err = String> + Default,
{
    value
        .map(str::parse)
        .transpose()
        .map(Option::unwrap_or_default)
        .map_err(|e| err(Status::BadRequest, e))
}

/// The restore route's query string, taken as one parameter.
///
/// Four options that arrive together and are consumed together, as
/// [`RestoreOptions`] -- a `FromForm` keeps them that way instead of spreading
/// them across the signature.
#[derive(FromForm, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RestoreQuery {
    /// Report what would happen, and change nothing.
    dry_run: Option<bool>,
    /// What to do with a package that is already here: `skip` (the default),
    /// `overwrite` or `merge-patches`.
    on_existing: Option<String>,
    /// Remove every package, setting and worker first.
    clear: Option<bool>,
    /// What to do with the secrets a dump carries: `ignore` (the default) or
    /// `copy`.
    secrets: Option<String>,
}

/// Restore a dump, returning before it has finished.
///
/// The whole archive is validated first, so a dump that would be refused is
/// refused before anything is written. A dry run answers with what it *would*
/// do and no job id, because there is nothing to watch.
///
/// Otherwise the rows are written in one transaction and the slow half -- one
/// source read per package, then the dependency graph -- runs detached, with
/// progress recorded the way a bulk add's is.
#[utoipa::path(
    request_body(content = Vec<u8>, description = "A dump, as `GET /dump` wrote it", content_type = "application/gzip"),
    params(RestoreQuery),
    responses(
            (status = 202, description = "Restore started, or for a dry run what it would do", body = RestoreAccepted),
            (status = 400, description = "Not a dump this server can read, or an unknown policy"),
            (status = 413, description = "Larger than any dump this server reads"),
    )
)]
#[post("/restore?<query..>", data = "<archive>")]
pub async fn restore(
    services: &State<Services>,
    ca_dir: &State<CaDirectory>,
    query: RestoreQuery,
    archive: Data<'_>,
    a: Authenticated,
) -> Result<status::Accepted<Json<RestoreAccepted>>, ApiError> {
    let RestoreQuery {
        dry_run,
        on_existing,
        clear,
        secrets,
    } = query;
    let bytes = archive
        .open(MAX_DUMP_SIZE)
        .into_bytes()
        .await
        .map_err(|e| err(Status::BadRequest, e))?;
    if !bytes.is_complete() {
        return Err(err(Status::PayloadTooLarge, "dump is too large"));
    }

    let options = RestoreOptions {
        dry_run: dry_run.unwrap_or(false),
        clear: clear.unwrap_or(false),
        secrets: parse_policy::<SecretsPolicy>(secrets.as_deref())?,
        on_existing: parse_policy::<ExistingPackagePolicy>(on_existing.as_deref())?,
    };

    // A dump that cannot be read is the caller's problem, and saying so before
    // anything is written is the point of validating the whole file first.
    let loaded =
        aurcache_utils::restore::load_dump(&bytes.value).map_err(|e| err(Status::BadRequest, e))?;
    let total = i32::try_from(loaded.packages.len()).unwrap_or(i32::MAX);

    if options.dry_run {
        let preview = aurcache_utils::restore::preview(&services.db, &loaded, &options).await?;
        return Ok(status::Accepted(Json(RestoreAccepted {
            job_id: None,
            total,
            preview,
        })));
    }

    let services_task = services.inner().clone();
    let ca_dir_task = ca_dir.inner().clone();
    let job_id = operation::start(
        services,
        operations::KIND_RESTORE,
        total,
        move |progress| async move {
            aurcache_utils::restore::apply(
                &services_task,
                &ca_dir_task.0,
                loaded,
                options,
                progress,
            )
            .await;
        },
        RestoreRecorder {
            activity: services.activity.clone(),
            username: a.username.clone(),
            succeeded: HashSet::new(),
            failed: HashSet::new(),
        },
    )
    .await?;

    Ok(status::Accepted(Json(RestoreAccepted {
        job_id: Some(job_id),
        total,
        preview: Vec::new(),
    })))
}

/// Counts a restore's packages, and logs how many it brought in.
///
/// Counted per package rather than per entry, because a package can be
/// reported twice: imported by the first pass, then failed by a later one when
/// its source turned out to be unreadable. The later word is the true one, so
/// it moves out of `completed` rather than adding to both. The log keeps both
/// lines -- the sequence is what explains what happened.
struct RestoreRecorder {
    activity: ActivityLog,
    username: Option<String>,
    succeeded: HashSet<String>,
    failed: HashSet<String>,
}

impl Recorder<RestoreEntry> for RestoreRecorder {
    fn record(&mut self, entry: &RestoreEntry) -> Counts {
        if let RestoreOutcome::Failed { .. } = entry.outcome {
            self.succeeded.remove(&entry.pkgbase);
            self.failed.insert(entry.pkgbase.clone());
        } else {
            self.succeeded.insert(entry.pkgbase.clone());
        }
        Counts {
            completed: i32::try_from(self.succeeded.len()).unwrap_or(i32::MAX),
            failed: i32::try_from(self.failed.len()).unwrap_or(i32::MAX),
        }
    }

    fn finish(self) {
        self.activity.emit_by(
            Event::RestoreApplied {
                packages: self.succeeded.len(),
            },
            self.username,
        );
    }
}

#[utoipa::path(
    responses(
            (status = 200, description = "Progress of a restore", body = OperationProgress<RestoreEntry>),
            (status = 404, description = "No such restore"),
    ),
    params(
        ("id", description = "Job id returned when the restore was started"),
        ("after", description = "How many entries the caller already holds"),
    )
)]
/// Read a restore's progress from `after` onwards.
#[get("/restore/<id>?<after>")]
pub async fn restore_progress(
    db: &State<DatabaseConnection>,
    id: i32,
    after: Option<usize>,
    _a: Authenticated,
) -> Result<Json<RestoreProgress>, ApiError> {
    operation::progress(db.inner(), id, operations::KIND_RESTORE, after.unwrap_or(0))
        .await
        .map(Json)
}

#[cfg(test)]
mod tests {
    use super::dump_file_name;

    /// Dated, so a directory of backups sorts chronologically and two dumps
    /// taken on different days do not overwrite each other.
    #[test]
    fn the_file_name_carries_the_date() {
        assert_eq!(
            dump_file_name(1_756_684_800),
            "aurcache-dump-20250901.tar.gz"
        );
    }
}
