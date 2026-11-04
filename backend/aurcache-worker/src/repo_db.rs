//! Conditional fetching of the repository database this worker serves, the
//! reference a reconcile holds the shared package cache against.
//!
//! The server publishes the DB over the repo port (`Server = …/$arch`), which
//! is the URL nobody wants to hardcode anywhere and this module never does:
//! it is derived from the `[repo]` section using
//! [`aurcache_worker_core::repo::RepoSection::db_url`]. `repo.db` changes on every
//! build (each completed build appends a package), so the DB is re-fetched
//! per job rather than on a timer — but conditionally: an `If-None-Match`
//! / `If-Modified-Since` round-trip costs a request and typically returns
//! `304`, which replays the already-parsed copy for free.
//!
//! The one rule that cannot bend: **a failure never returns a stale copy.**
//! Reconciling the cache against what we saw before is exactly how a fresh
//! stale file sneaks past — the caller facing a fetch error skips the
//! reconcile and lets the next job try again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use aurcache_worker_core::client::{ConditionalGet, WorkerClient};

use crate::cache;

/// What we last saw of one DB, kept so the next fetch can be conditional and
/// a `304` can reuse the work of parsing it.
///
/// Shared, not cloned: the steady state is an unchanged DB, and every job
/// would otherwise deep-clone the whole map on each `304`.
struct LastSeen {
    etag: Option<String>,
    last_modified: Option<String>,
    db: Arc<cache::RepoDb>,
}

/// The repository databases this worker has fetched, by URL, held for the
/// worker's lifetime.
///
/// When a fetch/parse fails the entry is left as it was, but a failure is
/// reported to the caller as an error and the caller skips the reconcile --
/// so a `304` only ever follows a successful previous fetch, and the parsed
/// map mirrored here is only ever what the server truly sent.
#[derive(Default)]
pub struct RepoDbs {
    last_seen: Mutex<HashMap<String, LastSeen>>,
}

impl RepoDbs {
    fn last_seen(&self) -> MutexGuard<'_, HashMap<String, LastSeen>> {
        self.last_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Fetch the repository DB for `arch` from where the client's `[repo]`
    /// section points, and parse it, conditionally.
    ///
    /// No section, or one pointing nowhere the worker can fetch from, is an
    /// empty map, never an error: there is nothing to reconcile against. A
    /// `200` replaces the stored copy, a `304` replays it, and anything else
    /// -- a connection failure, a non-`200/304` status, an unparseable body --
    /// is an `Err` carrying the URL in the message. The caller decides the
    /// response, but the one response the design forbids is silently
    /// proceeding with the old copy.
    pub async fn fetch(&self, client: &WorkerClient, arch: &str) -> Result<Arc<cache::RepoDb>> {
        let Some(url) = client.repo().and_then(|repo| repo.db_url(arch)) else {
            return Ok(Arc::default());
        };
        let (etag, last_modified) = {
            let last = self.last_seen();
            match last.get(&url) {
                Some(seen) => (seen.etag.clone(), seen.last_modified.clone()),
                None => (None, None),
            }
        };
        let got: ConditionalGet = client
            .get_conditional(&url, etag.as_deref(), last_modified.as_deref())
            .await
            .with_context(|| format!("fetching {url}"))?;
        let Some(bytes) = got.body else {
            // The steady-state path: a refcount bump, not a map clone.
            return Ok(self
                .last_seen()
                .get(&url)
                .map(|seen| Arc::clone(&seen.db))
                .unwrap_or_default());
        };
        let db = Arc::new(cache::parse_repo_db(&bytes).with_context(|| format!("parsing {url}"))?);
        self.last_seen().insert(
            url,
            LastSeen {
                etag: got.etag,
                last_modified: got.last_modified,
                db: Arc::clone(&db),
            },
        );
        Ok(db)
    }
}
