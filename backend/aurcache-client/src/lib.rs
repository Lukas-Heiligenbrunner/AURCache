//! Typed Rust client for the AURCache HTTP API.
//!
//! This crate exposes request/response models for the API together with
//! [`AurCacheClient`], a small async wrapper around the most common endpoints.

use anyhow::{Context, Result};
use reqwest::Url;
// The API shapes are defined once, in aurcache-common, and used by the server,
// this client, and the browser frontend alike. What is declared below is the
// client's own: how it builds requests and reports failures.
pub use aurcache_common::api::activity::Severity;
pub use aurcache_common::api::builds::BuildSummary as Build;
pub use aurcache_common::api::dump::{
    ExistingPackagePolicy, RestoreAccepted, RestoreEntry, RestoreOptions, RestoreOutcome,
    RestoreProgress, SecretsPolicy,
};
pub use aurcache_common::api::events::{Event, KINDS, Kind, Segment, kind_label};
pub use aurcache_common::api::info::{ServerInfo, Timezone};
pub use aurcache_common::api::log::{
    BuildRef, EntityRef, LogEntry, LogFilter, LogPage, PackageRef, WorkerRef,
};
pub use aurcache_common::api::operations::{
    ActiveOperation, OperationProgress, kind as operation_kind,
};
pub use aurcache_common::api::package::{
    AddPackages as AddPackagesRequest, BulkAddAccepted, BulkAddEntry, BulkAddOutcome,
    BulkAddProgress, ExtendedPackage, PackageDependency, PackageFile, SimplePackage,
};
pub use aurcache_common::api::package::{
    AurNotFoundPackage, AurPackage, PackagePatch, PackageSource, UpdatePackage, UploadPackage,
};
pub use aurcache_common::api::package::{
    CandidateSource, DependencyCandidate, DependencyOptions, ReplaceDependency, ReplacementVerdict,
};
// The add and preview requests are the server's own shapes rather than copies,
// so a field added to one cannot be silently absent from the other.
pub use aurcache_common::api::package::{
    AddPackage as AddPackageRequest, SourcePreviewFileRequest, SourcePreviewRequest,
};
pub use aurcache_common::api::package::{SourceFileContent, SourceFileList, SourceFileUpdate};
pub use aurcache_common::api::repo::RepoInfo;
pub use aurcache_common::api::settings::{SchedulePreview, SettingResponse, SettingValue};
pub use aurcache_common::api::stats::{
    ApiTokenResponse, DashboardView, GraphDataPoint, ListStats, LongBuild, OutOfDateSlice,
    QueueSlice, UserInfo,
};
pub use aurcache_common::api::waiting::WaitingReason;
pub use aurcache_common::api::worker::{
    ApprovalStatus, WorkerConfigUpdate, WorkerConfigView, WorkerJoinInfo, WorkerSummary as Worker,
};
pub use aurcache_common::settings::{
    ApplicationSettings, Setting, SettingSource, SettingsEntry, SettingsMeta,
};
pub use aurcache_common::source::{GitSourceSpec, SourceData, looks_like_git_url, source_label};
use reqwest::Response;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;

/// Re-export of [`reqwest::Method`] for generic request helpers.
pub use reqwest::Method;

/// Search result returned from the AUR search proxy endpoint.
///
/// The server's own shape rather than a copy of it, so a field added to one
/// cannot silently be missing from the other.
pub use aurcache_common::api::aur::ApiPackage as SearchResult;

/// Where a request for the API actually landed.
///
/// Reaching *something* is not the same as reaching the API. Pointed at the
/// web UI, the server answers `/health` with a redirect to its login page, and
/// that page returns a perfectly good 200 -- so a bare status check calls the
/// URL healthy and the mistake resurfaces as an unrelated complaint about the
/// token at the next call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiReachability {
    /// The API answered directly.
    Api,
    /// Something answered, but not the API, and `detail` says how that was
    /// established: the request was redirected away, or the answer was a web
    /// page where the API returns none.
    NotApi { detail: String },
}

/// How long to wait for a TCP+TLS connection to establish before giving up.
#[cfg(not(target_arch = "wasm32"))]
const CLIENT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// How long one request may take overall. Every call through this client is a
/// quick API round trip — except a dump, which overrides this per request —
/// so two minutes is a hung socket, not a slow server.
const CLIENT_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// How long a dump may take to travel either way, the restore upload and the
/// download alike: up to 64 MiB on a slow link.
const CLIENT_UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Async HTTP client for the AURCache API.
///
/// Cheap to clone (the pool is shared): screens hold one per resource, and the
/// browser frontend keeps a single process-wide client.
#[derive(Clone)]
pub struct AurCacheClient {
    /// The API base as a parsed URL (with trailing slash), so per-request paths
    /// join onto it instead of rebuilding and re-parsing a string on every
    /// call — including the 1s/5s poll loops.
    base: reqwest::Url,
    token: Option<String>,
    client: reqwest::Client,
    on_unauthorized: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl AurCacheClient {
    /// Creates a new client for the given base URL and optional bearer token.
    ///
    /// `base_url` should normally point at the API root, for example
    /// `http://localhost:8080/api`.
    pub fn new(base_url: String, token: Option<String>) -> Result<Self> {
        // Parsed once, including the trailing slash `Url::join` needs to keep
        // the `/api` prefix. An unparseable base fails here rather than on
        // every request.
        let mut joined = base_url.trim_end_matches('/').to_string();
        joined.push('/');
        let base = reqwest::Url::parse(&joined).context("invalid API base URL")?;
        // A hung socket must not hang the caller forever: without deadlines
        // one stalled connection stops any CLI command (or browser poll loop)
        // with no lease watchdog to save it. The total timeout does not cover
        // a dump -- a tens-of-MB body on a slow link legitimately takes
        // minutes -- so the dump download, the restore upload and a build log
        // page override it per request (the same reason the worker protocol
        // client keeps a deadline-free upload client).
        //
        // The total is set per request in `request`, which is the only form
        // reqwest's wasm client offers; natively the builder bounds the
        // connect phase as well, which the browser's fetch has no separate
        // handle on.
        let builder = reqwest::Client::builder();
        #[cfg(not(target_arch = "wasm32"))]
        let builder = builder
            .connect_timeout(CLIENT_CONNECT_TIMEOUT)
            .timeout(CLIENT_REQUEST_TIMEOUT);
        Ok(Self {
            base,
            token,
            client: builder.build().context("failed to build HTTP client")?,
            on_unauthorized: None,
        })
    }

    /// Runs `handler` once each time a response comes back as a 401.
    ///
    /// A caller in a context where a 401 means "the session is gone" — the
    /// browser frontend, whose session is a cookie the server can no longer
    /// decode after a restart — can use this to send the user to the login flow
    /// instead of leaving the failure to surface as a scattered error message.
    /// Clients that handle 401s themselves, like the CLI, simply never set it.
    #[must_use]
    pub fn on_unauthorized(mut self, handler: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_unauthorized = Some(Arc::new(handler));
        self
    }

    /// The API base URL this client was built with.
    ///
    /// Exposed because tooling has to talk about the *deployment* as well as
    /// the API: the pacman repository and the worker protocol are the same host
    /// on other ports, and this is where that host comes from.
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    /// How this instance publishes its pacman repository.
    pub async fn repo_info(&self) -> Result<RepoInfo> {
        self.get("/repo/info").await
    }

    /// Calls the health endpoint and returns success when the instance is healthy.
    pub async fn health(&self) -> Result<()> {
        match self.probe_api().await? {
            ApiReachability::Api => Ok(()),
            ApiReachability::NotApi { detail } => Err(anyhow::anyhow!(
                "{} answered, but {detail}: that is the web UI, not the API",
                self.url("/health")?
            )),
        }
    }

    /// Ask the health endpoint whether this URL is the API, and where the
    /// request ended up if it is not.
    pub async fn probe_api(&self) -> Result<ApiReachability> {
        let requested = self.url("/health")?;
        let response = self.request(Method::GET, "/health").send().await?;
        let landed_on = response.url().clone();
        if !same_endpoint(&requested, &landed_on) {
            return Ok(ApiReachability::NotApi {
                detail: format!("the request was redirected to {landed_on}"),
            });
        }
        // Not every wrong URL redirects. The UI is a single-page app served by
        // the same host, so with a token in hand `/health` comes back as a
        // perfectly good 200 -- carrying `index.html`. The API answers it with
        // an empty body, so the page itself is the tell.
        let body = response
            .text()
            .await
            .with_context(|| format!("failed to read the response from {requested}"))?;
        Ok(if looks_like_html(&body) {
            ApiReachability::NotApi {
                detail: "the answer was a web page".to_string(),
            }
        } else {
            ApiReachability::Api
        })
    }

    /// Asks the server for its own version: the release, plus the commit when
    /// the build is not exactly that release. The bundled UI shows this
    /// rather than its own crate version, so the two agree about what is
    /// running.
    pub async fn server_info(&self) -> Result<ServerInfo> {
        self.get("/version").await
    }

    /// Fetches information about the currently authenticated user.
    pub async fn user_info(&self) -> Result<UserInfo> {
        self.get("/userinfo").await
    }

    /// Fetches aggregate dashboard statistics.
    pub async fn stats(&self) -> Result<ListStats> {
        self.get("/stats").await
    }

    /// Fetches monthly dashboard graph datapoints.
    pub async fn graph(&self) -> Result<Vec<GraphDataPoint>> {
        self.get("/graph").await
    }

    /// Fetches every dashboard card in one response, beside `stats`/`graph`.
    pub async fn dashboard(&self) -> Result<DashboardView> {
        self.get("/stats/dashboard").await
    }

    /// Searches for packages through the AUR-backed search endpoint.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        #[derive(Serialize)]
        struct Search<'a> {
            query: &'a str,
        }
        self.get_query("/search", &Search { query }).await
    }

    /// Regenerates the authenticated user's API token.
    pub async fn regenerate_api_token(&self) -> Result<ApiTokenResponse> {
        self.request(Method::POST, "/token/regenerate")
            .send_json()
            .await
    }

    /// List packages using pagination and an explicit dependency selection.
    pub async fn list_packages(&self, query: &PackageQuery) -> Result<Vec<SimplePackage>> {
        self.get_query("/packages/list", query).await
    }

    /// Fetches the details of one package, by pkgbase.
    pub async fn get_package(&self, pkgbase: &str) -> Result<ExtendedPackage> {
        self.get(&format!("/package/{pkgbase}")).await
    }

    /// Adds a package using the supplied request payload.
    pub async fn add_package(&self, body: &AddPackageRequest) -> Result<()> {
        self.request(Method::POST, "/package")
            .json(body)
            .send_empty()
            .await
    }

    /// Starts adding several packages in one request, returning before any of
    /// them are added.
    ///
    /// The server resolves every AUR name to its pkgbase in one batched RPC
    /// call, so this is not the same as calling [`Self::add_package`] in a
    /// loop: adding N packages that way costs N AUR requests for the
    /// resolution alone. Poll [`Self::bulk_add_progress`] with the returned id
    /// to follow it, or do not -- the work does not depend on being watched.
    pub async fn add_packages(&self, body: &AddPackagesRequest) -> Result<BulkAddAccepted> {
        self.request(Method::POST, "/packages")
            .json(body)
            .send_json()
            .await
    }

    /// Reads a bulk add's progress, returning only the entries after the first
    /// `after` of them.
    ///
    /// Pass the number already held so each poll returns just what is new;
    /// pass zero to get the run from the beginning, including whatever happened
    /// before this caller started watching.
    pub async fn bulk_add_progress(&self, job_id: i32, after: usize) -> Result<BulkAddProgress> {
        self.get_query(&format!("/packages/bulk/{job_id}"), &After { after })
            .await
    }

    /// Triggers an update check for the given package.
    ///
    /// Returns the build number, within that package, of each build queued as
    /// a result of the update — one per platform that got one.
    pub async fn update_package(&self, pkgbase: &str, body: &UpdatePackage) -> Result<Vec<i32>> {
        self.request(Method::POST, &format!("/package/{pkgbase}/update"))
            .json(body)
            .send_json()
            .await
    }

    /// Partially updates package metadata.
    pub async fn patch_package(&self, pkgbase: &str, body: &PackagePatch) -> Result<()> {
        self.request(Method::PATCH, &format!("/package/{pkgbase}"))
            .json(body)
            .send_empty()
            .await
    }

    /// What could take over one of a package's dependencies.
    pub async fn dependency_options(
        &self,
        pkgbase: &str,
        dependency: &str,
    ) -> Result<DependencyOptions> {
        self.get(&format!(
            "/package/{pkgbase}/dependency/{dependency}/options"
        ))
        .await
    }

    /// Points one of a package's dependencies at `replacement`, or drops the
    /// dependency entirely when it is `None`.
    ///
    /// Dropping is accepted only where the official repositories publish the
    /// name, since anything else would be undone at the next update.
    pub async fn replace_dependency(
        &self,
        pkgbase: &str,
        dependency: &str,
        replacement: Option<&str>,
    ) -> Result<()> {
        self.request(
            Method::PUT,
            &format!("/package/{pkgbase}/dependency/{dependency}"),
        )
        .json(&ReplaceDependency {
            replacement: replacement.map(ToString::to_string),
        })
        .send_empty()
        .await
    }

    /// Removes the direct-request flag from the given package.
    pub async fn delete_package(&self, pkgbase: &str) -> Result<()> {
        self.delete(&format!("/package/{pkgbase}")).await
    }

    /// Lists builds, newest first, narrowed by `query`.
    pub async fn list_builds(&self, query: &BuildQuery) -> Result<Vec<Build>> {
        // Builds for one package are a sub-resource, not a query filter: a
        // pkgbase may contain `+`, which is literal in a path but decodes to a
        // space in a query value.
        let path = match &query.pkgbase {
            Some(pkgbase) => format!("/package/{pkgbase}/builds"),
            None => "/builds".to_string(),
        };
        self.get_query(&path, query).await
    }

    /// Lists the files in a package's source tree.
    pub async fn list_source_files(&self, pkgbase: &str) -> Result<SourceFileList> {
        self.get(&format!("/package/{pkgbase}/source/files")).await
    }

    /// Reads one source file, pristine and patched.
    pub async fn get_source_file(&self, pkgbase: &str, path: &str) -> Result<SourceFileContent> {
        #[derive(Serialize)]
        struct File<'a> {
            path: &'a str,
        }
        self.get_query(&format!("/package/{pkgbase}/source/file"), &File { path })
            .await
    }

    /// Replaces one source file's content. The server stores the difference
    /// from the pristine source as the package's patch; writing back the
    /// original content is therefore how a file is un-patched.
    pub async fn put_source_file(&self, pkgbase: &str, path: &str, content: &str) -> Result<()> {
        let body = SourceFileUpdate {
            path: path.to_string(),
            content: content.to_string(),
        };
        self.request(Method::PUT, &format!("/package/{pkgbase}/source/file"))
            .json(&body)
            .send_empty()
            .await
    }

    /// Fetch one build by its public identity, `<pkgbase>/<number>`.
    pub async fn get_build(&self, pkgbase: &str, number: i32) -> Result<Build> {
        self.get(&format!("/package/{pkgbase}/build/{number}"))
            .await
    }

    /// Long-running operations still in flight.
    ///
    /// A bulk add or restore outlives the page that started it, so this is how
    /// a browser finds one again -- after a reload, or from a tab that never
    /// started it.
    pub async fn active_operations(&self) -> Result<Vec<ActiveOperation>> {
        self.get("/operations").await
    }

    /// Fetches one page of a build's raw log output, decoded lossily to text.
    ///
    /// A single-request convenience over [`Self::build_output_page`]: the log
    /// is stored and transferred as raw bytes, and alignment is the caller's
    /// job, so a page that starts or ends inside a multi-byte character comes
    /// back with a replacement character at that boundary. Callers that walk
    /// the log in pages should use [`Self::build_output_page`] and own the
    /// offsets, so trailing characters re-read whole.
    pub async fn build_output(
        &self,
        pkgbase: &str,
        number: i32,
        offset: Option<u64>,
    ) -> Result<String> {
        Ok(String::from_utf8_lossy(
            &self
                .build_output_page(pkgbase, number, offset, None)
                .await?,
        )
        .into_owned())
    }

    /// Fetch one bounded page of a build's log as raw bytes.
    ///
    /// `offset` and `limit` are byte counts, both optional: the server starts
    /// at `offset` and returns at most `limit` bytes, defaulting `limit` to —
    /// and clamping it to — its own bound. The body is the raw file bytes, so
    /// a page may start or end mid-character; the caller owns alignment by
    /// passing `next_offset = offset + page.len() - back_drop` where
    /// `back_drop` is how much of a trailing character the page dropped, which
    /// re-reads that character whole on the next call. An empty `Ok(vec![])`
    /// page means the log has nothing new at that offset.
    pub async fn build_output_page(
        &self,
        pkgbase: &str,
        number: i32,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<Vec<u8>> {
        #[derive(Serialize)]
        struct Page {
            #[serde(skip_serializing_if = "Option::is_none")]
            offset: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            limit: Option<u64>,
        }
        self.request(
            Method::GET,
            &format!("/package/{pkgbase}/build/{number}/output"),
        )
        .query(&Page { offset, limit })
        // A page is up to 32 MiB, so it gets the long deadline a dump does.
        .timeout(CLIENT_UPLOAD_TIMEOUT)
        .send_bytes()
        .await
    }

    /// Re-runs the given build, returning the new build's number within the
    /// same package.
    pub async fn retry_build(&self, pkgbase: &str, number: i32) -> Result<i32> {
        self.request(
            Method::POST,
            &format!("/package/{pkgbase}/build/{number}/retry"),
        )
        .send_json()
        .await
    }

    /// Requests cancellation of the given build.
    pub async fn cancel_build(&self, pkgbase: &str, number: i32) -> Result<()> {
        self.post_empty(&format!("/package/{pkgbase}/build/{number}/cancel"))
            .await
    }

    /// Deletes the given build record.
    pub async fn delete_build(&self, pkgbase: &str, number: i32) -> Result<()> {
        self.delete(&format!("/package/{pkgbase}/build/{number}"))
            .await
    }

    /// Lists the files of a source that has not been added yet.
    ///
    /// The source travels in the body rather than being named by pkgbase,
    /// because there is no package to name: this is what lets a PKGBUILD be
    /// inspected — and fixed — before the package exists.
    pub async fn preview_source_files(&self, source: &SourceData) -> Result<SourceFileList> {
        self.request(Method::POST, "/package/source/preview/files")
            .json(&SourcePreviewRequest {
                source: source.clone(),
            })
            .send_json()
            .await
    }

    /// Reads one pristine file from a source that has not been added yet.
    pub async fn preview_source_file(
        &self,
        source: &SourceData,
        path: &str,
    ) -> Result<SourceFileContent> {
        self.request(Method::POST, "/package/source/preview/file")
            .json(&SourcePreviewFileRequest {
                source: source.clone(),
                path: path.to_string(),
            })
            .send_json()
            .await
    }

    /// Fetches every setting with the value in force and where it came from.
    ///
    /// Pass a `pkgbase` for the per-package view, where a setting the package
    /// does not override reports the global value it inherits.
    pub async fn settings(&self, pkgbase: Option<&str>) -> Result<ApplicationSettings> {
        self.get(&settings_path(pkgbase, None)).await
    }

    /// Reads one setting: the value in force, and where it came from.
    ///
    /// Separate from [`Self::settings`] because not every setting is in that
    /// response — the config files are large text blobs, and sending both of
    /// them with every settings fetch would be wasteful.
    pub async fn get_setting(&self, pkgbase: Option<&str>, key: &str) -> Result<SettingResponse> {
        self.get(&settings_path(pkgbase, Some(key))).await
    }

    /// Stores a value for one setting, overriding whatever it inherits.
    ///
    /// The value is sent as a string whatever its type: the server owns the
    /// parsing, so a client that formats a number differently cannot store
    /// something the server would reject on read.
    pub async fn patch_setting(&self, pkgbase: Option<&str>, key: &str, value: &str) -> Result<()> {
        self.request(Method::PATCH, &settings_path(pkgbase, Some(key)))
            .json(&SettingValue {
                value: value.to_string(),
            })
            .send_empty()
            .await
    }

    /// When `value` would run, for a setting that holds a schedule; nothing is
    /// stored. The server answers because only it knows its timezone and what
    /// `H` resolves to on this instance.
    pub async fn preview_schedule(&self, key: &str, value: &str) -> Result<SchedulePreview> {
        self.request(
            Method::POST,
            &format!("{}/schedule", settings_path(None, Some(key))),
        )
        .json(&SettingValue {
            value: value.to_string(),
        })
        .send_json()
        .await
    }

    /// Drops this scope's stored value, so the setting inherits again.
    pub async fn reset_setting(&self, pkgbase: Option<&str>, key: &str) -> Result<()> {
        self.delete(&settings_path(pkgbase, Some(key))).await
    }

    /// One page of the log, newest first, with how long the filtered log is.
    ///
    /// Paged *and filtered* on the server because the log only grows: there is
    /// no point at which fetching all of it is the cheap option, and a filter
    /// applied after the fact would only search the page it was given.
    pub async fn log(
        &self,
        limit: Option<u64>,
        offset: Option<u64>,
        filter: &LogFilter,
    ) -> Result<LogPage> {
        #[derive(Serialize)]
        struct Log<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            limit: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            offset: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            severity: Option<&'static str>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            since_boot: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            kind: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            entity: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            role: Option<&'a str>,
        }
        let query = Log {
            limit,
            offset,
            severity: filter.severity.map(Severity::slug),
            since_boot: filter.since_boot,
            kind: filter.kind.as_deref(),
            entity: filter.entity.as_ref().map(ToString::to_string),
            role: filter.role.as_deref(),
        };
        self.get_query("/log", &query).await
    }

    /// Lists all enrolled remote build workers and their status.
    pub async fn list_workers(&self) -> Result<Vec<Worker>> {
        self.get("/workers").await
    }

    /// What one worker declares it can be configured with, and what it reports
    /// it is running.
    ///
    /// Apart from [`Self::list_workers`] because it is fetched for the one
    /// worker being looked at, while the list is polled for the whole fleet.
    pub async fn worker_config(&self, id: i32) -> Result<WorkerConfigView> {
        self.get(&format!("/workers/{id}/config")).await
    }

    /// Saves values for several of a worker's settings as one change, and
    /// returns the configuration as it stands afterwards. `None` removes a
    /// value so the worker falls back to its own.
    pub async fn update_worker_config(
        &self,
        id: i32,
        update: &WorkerConfigUpdate,
    ) -> Result<WorkerConfigView> {
        self.request(Method::PATCH, &format!("/workers/{id}/config"))
            .json(update)
            .send_json()
            .await
    }

    /// The image and worker-protocol port a new worker's `docker run` needs.
    pub async fn worker_join_info(&self) -> Result<WorkerJoinInfo> {
        self.get("/workers/join-info").await
    }

    /// Approves a pending worker, signing its CSR so it can build.
    pub async fn approve_worker(&self, id: i32) -> Result<()> {
        self.post_empty(&format!("/workers/{id}/approve")).await
    }

    /// Asks a worker to take no new builds and let the ones it holds finish.
    pub async fn pause_worker(&self, id: i32) -> Result<()> {
        self.post_empty(&format!("/workers/{id}/pause")).await
    }

    /// Lets a paused worker take builds again.
    pub async fn resume_worker(&self, id: i32) -> Result<()> {
        self.post_empty(&format!("/workers/{id}/resume")).await
    }

    /// Revokes a worker, immediately refusing its certificate on the next call.
    pub async fn revoke_worker(&self, id: i32) -> Result<()> {
        self.post_empty(&format!("/workers/{id}/revoke")).await
    }

    /// A request for `path`, to describe further and then send.
    pub fn request(&self, method: Method, path: &str) -> Request<'_> {
        let builder = self.url(path).map(|url| {
            let request = self
                .client
                .request(method, url)
                .timeout(CLIENT_REQUEST_TIMEOUT);
            match &self.token {
                Some(token) => request.bearer_auth(token),
                None => request,
            }
        });
        Request {
            client: self,
            path: path.to_string(),
            builder,
        }
    }

    /// `GET path`, decoded as JSON.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.request(Method::GET, path).send_json().await
    }

    /// `GET path?query`, decoded as JSON.
    pub async fn get_query<T, Q>(&self, path: &str, query: &Q) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        self.request(Method::GET, path)
            .query(query)
            .send_json()
            .await
    }

    /// `POST path` with no body, for an endpoint that answers with none.
    pub async fn post_empty(&self, path: &str) -> Result<()> {
        self.request(Method::POST, path).send_empty().await
    }

    /// `DELETE path`, for an endpoint that answers with no body.
    pub async fn delete(&self, path: &str) -> Result<()> {
        self.request(Method::DELETE, path).send_empty().await
    }

    /// Restore a dump, returning before it has finished.
    ///
    /// A dry run reports what it *would* do and no job id, because it changed
    /// nothing to watch.
    pub async fn restore(
        &self,
        archive: bytes::Bytes,
        options: &RestoreOptions,
    ) -> Result<RestoreAccepted> {
        /// The query spelling of [`RestoreOptions`], which is not its JSON one.
        #[derive(Serialize)]
        struct Restore {
            dry_run: bool,
            on_existing: &'static str,
            clear: bool,
            secrets: &'static str,
        }
        let query = Restore {
            dry_run: options.dry_run,
            on_existing: options.on_existing.as_str(),
            clear: options.clear,
            secrets: options.secrets.as_str(),
        };
        // A dump is tens of MB on a slow link, so the upload gets a deadline
        // of its own.
        self.request(Method::POST, "/restore")
            .query(&query)
            .raw(archive, "application/gzip")
            .timeout(CLIENT_UPLOAD_TIMEOUT)
            .send_json()
            .await
    }

    /// Read a restore's progress, returning only entries after the first
    /// `after` of them.
    pub async fn restore_progress(&self, job_id: i32, after: usize) -> Result<RestoreProgress> {
        self.get_query(&format!("/restore/{job_id}"), &After { after })
            .await
    }

    /// Download a lite export of the server's authored state.
    ///
    /// `include_secrets` adds the CA private key, the worker certificates and
    /// the API token hashes. The resulting file can mint worker identities this
    /// server accepts; treat it as a credential.
    pub async fn dump(&self, include_secrets: bool) -> Result<Vec<u8>> {
        #[derive(Serialize)]
        struct Dump {
            include_secrets: bool,
        }
        // As large as the restore upload, so the same deadline.
        self.request(Method::GET, "/dump")
            .query(&Dump { include_secrets })
            .timeout(CLIENT_UPLOAD_TIMEOUT)
            .send_bytes()
            .await
    }

    /// `path` against the API base. Absolute URLs pass through.
    fn url(&self, path: &str) -> Result<Url> {
        if path.starts_with("http://") || path.starts_with("https://") {
            Url::parse(path)
        } else {
            self.base.join(path.trim_start_matches('/'))
        }
        .with_context(|| format!("invalid URL for path {path}"))
    }
}

/// Whether a failed call failed because the API refused the session or token
/// it was presented with: the server answered 401.
#[must_use]
pub fn is_unauthorized(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<ApiError>()
            .is_some_and(ApiError::is_unauthorized)
    })
}

/// Whether a response came back from the endpoint that was asked for.
///
/// Compared by scheme, host, port and path rather than as text: an empty query
/// string is spelled with a trailing `?` in the response's URL and not in ours,
/// and a trailing slash is not a different endpoint by any useful definition.
/// Textual comparison calls both a redirect and reports a correct URL as the
/// web UI.
fn same_endpoint(requested: &Url, landed_on: &Url) -> bool {
    requested.scheme() == landed_on.scheme()
        && requested.host_str() == landed_on.host_str()
        && requested.port_or_known_default() == landed_on.port_or_known_default()
        && requested.path().trim_end_matches('/') == landed_on.path().trim_end_matches('/')
}

/// Why a JSON decode failed, in terms the caller can act on.
///
/// A body opening with `<` is the web UI's HTML, which means the base URL names
/// the site rather than the API. That is the common way to arrive here, and
/// serde describes it as "expected value at line 1 column 1" -- an accurate
/// account of the first byte and no help at all.
fn json_decode_context(path: &str, body: &str) -> String {
    if looks_like_html(body) {
        format!(
            "expected JSON from {path} but the server returned an HTML page: the URL looks \
             like the web UI rather than the API (try adding `/api`)"
        )
    } else {
        format!("failed to decode JSON response from {path}")
    }
}

/// A progress poll: the entries after the first `after`.
#[derive(Serialize)]
struct After {
    after: usize,
}

/// A request to the API, sent by one of its `send_*` methods, each of which
/// fails on an answer that is not a success.
#[must_use]
pub struct Request<'c> {
    client: &'c AurCacheClient,
    path: String,
    /// An unparseable URL fails at sending, with the rest.
    builder: Result<reqwest::RequestBuilder>,
}

impl Request<'_> {
    /// With `query` as the URL's query string.
    pub fn query<Q: Serialize + ?Sized>(self, query: &Q) -> Self {
        self.with(|request| request.query(query))
    }

    /// With `body` as a JSON body.
    pub fn json<B: Serialize + ?Sized>(self, body: &B) -> Self {
        self.with(|request| request.json(body))
    }

    /// With `bytes` as the body, as they are.
    fn raw(self, bytes: bytes::Bytes, content_type: &'static str) -> Self {
        self.with(|request| {
            request
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(bytes)
        })
    }

    /// With `timeout` as the whole request's deadline, in place of the default
    /// for an API round trip.
    fn timeout(self, timeout: std::time::Duration) -> Self {
        self.with(|request| request.timeout(timeout))
    }

    fn with(self, change: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> Self {
        Self {
            builder: self.builder.map(change),
            ..self
        }
    }

    /// Send it, failing on an answer that is not a success, and run the
    /// client's `on_unauthorized` callback when the server refused the
    /// session. Every request goes through here, so none can miss the
    /// callback.
    async fn send(self) -> Result<Response> {
        let response = self.builder?.send().await.context("request failed")?;
        match ensure_success(response).await {
            Ok(response) => Ok(response),
            Err(error) => {
                if is_unauthorized(&error)
                    && let Some(on_unauthorized) = &self.client.on_unauthorized
                {
                    on_unauthorized();
                }
                Err(error)
            }
        }
    }

    /// Send it, and decode the answer as JSON.
    ///
    /// The body is read first: `json()` consumes the response, leaving nothing
    /// to explain the failure with beyond serde's "expected value at line 1
    /// column 1", which describes the symptom and not the cause.
    pub async fn send_json<T: DeserializeOwned>(self) -> Result<T> {
        let path = self.path.clone();
        let body = self.send_text().await?;
        serde_json::from_str(&body).with_context(|| json_decode_context(&path, &body))
    }

    /// Send it, to an endpoint that answers with no meaningful body.
    pub async fn send_empty(self) -> Result<()> {
        self.send().await?;
        Ok(())
    }

    /// Send it, and return the answer as text.
    pub async fn send_text(self) -> Result<String> {
        let path = self.path.clone();
        self.send()
            .await?
            .text()
            .await
            .with_context(|| format!("failed to read the response from {path}"))
    }

    /// Send it, and return the answer as bytes: a dump is a `.tar.gz`, which
    /// decoding as UTF-8 would corrupt.
    pub async fn send_bytes(self) -> Result<Vec<u8>> {
        let path = self.path.clone();
        Ok(self
            .send()
            .await?
            .bytes()
            .await
            .with_context(|| format!("failed to read the response from {path}"))?
            .to_vec())
    }
}

/// Settings are a sub-resource of the package in the per-package scope, so
/// every settings call has the same two shapes.
fn settings_path(pkgbase: Option<&str>, key: Option<&str>) -> String {
    let mut path = match pkgbase {
        Some(pkgbase) => format!("/package/{pkgbase}/settings"),
        None => "/settings".to_string(),
    };
    if let Some(key) = key {
        path.push('/');
        path.push_str(key);
    }
    path
}

/// Which builds [`AurCacheClient::list_builds`] returns. Every field
/// left unset matches everything.
///
/// Serialised as the query string, but for `pkgbase`, which is the path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BuildQuery {
    /// Builds of this package only.
    #[serde(skip)]
    pub pkgbase: Option<String>,
    /// Builds this worker (by id) ran, or is running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<i32>,
    /// Builds in one of these states. Empty is every state.
    #[serde(
        rename = "status",
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "state_keys"
    )]
    pub states: Vec<aurcache_common::build_state::BuildState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u64>,
}

/// States as the server takes them: their keys, comma-separated.
fn state_keys<S: serde::Serializer>(
    states: &[aurcache_common::build_state::BuildState],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    let keys: Vec<&str> = states.iter().map(|state| state.key()).collect();
    serializer.serialize_str(&keys.join(","))
}

async fn ensure_success(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response.text().await.unwrap_or_default();
    Err(ApiError::from_response(status, &body).into())
}

/// A structured error for a non-2xx API response.
///
/// Server error pages are not guaranteed to be plain text (Rocket's default
/// error pages, for instance, are HTML), so this normalizes the body into a
/// short, human-readable message instead of dumping raw markup, and exposes
/// the status code so callers (like the CLI) can react to specific cases
/// such as authentication failures.
#[derive(Debug)]
pub struct ApiError {
    pub status: reqwest::StatusCode,
    pub message: String,
}

impl ApiError {
    fn from_response(status: reqwest::StatusCode, body: &str) -> Self {
        let message = if status == reqwest::StatusCode::UNAUTHORIZED {
            "Authentication failed: missing or invalid API token".to_string()
        } else {
            normalize_error_body(body)
        };
        Self { status, message }
    }

    /// Whether this error corresponds to an HTTP 401 Unauthorized response.
    pub fn is_unauthorized(&self) -> bool {
        self.status == reqwest::StatusCode::UNAUTHORIZED
    }

    /// Whether the server said the thing asked for does not exist.
    pub fn is_not_found(&self) -> bool {
        self.status == reqwest::StatusCode::NOT_FOUND
    }
}

/// Whether a failed call failed because the thing it named does not exist:
/// the server answered 404, wherever in the error's chain that answer sits.
#[must_use]
pub fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<ApiError>()
            .is_some_and(ApiError::is_not_found)
    })
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(f, "request failed with HTTP {}", self.status)
        } else {
            write!(
                f,
                "request failed with HTTP {}: {}",
                self.status, self.message
            )
        }
    }
}

impl std::error::Error for ApiError {}

fn normalize_error_body(body: &str) -> String {
    /// What enters the error chain: a proxy's error page must not become the
    /// whole error. Cut on a char boundary, and say so.
    const MAX_ERROR_BODY_CHARS: usize = 2048;
    let message = normalize_full_body(body);
    match message
        .char_indices()
        .nth(MAX_ERROR_BODY_CHARS)
        .map(|(idx, _)| idx)
    {
        Some(idx) => format!("{}… [truncated]", &message[..idx]),
        None => message,
    }
}

fn normalize_full_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(message) = serde_json::from_str::<String>(trimmed) {
        return message;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return serde_json::to_string_pretty(&value).unwrap_or_else(|_| trimmed.to_string());
    }
    // Fall back for non-JSON bodies: avoid dumping raw HTML error pages
    // (e.g. Rocket's default catchers) in favor of a short, readable message.
    if looks_like_html(trimmed) {
        return String::new();
    }
    trimmed.to_string()
}

fn looks_like_html(body: &str) -> bool {
    let lower = body.trim_start().to_ascii_lowercase();
    lower.starts_with("<!doctype html") || lower.starts_with("<html")
}

/// Which packages to include in a listing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DependencySelection {
    #[default]
    RequestedOnly,
    Include,
}

impl DependencySelection {
    const fn is_requested_only(&self) -> bool {
        matches!(self, Self::RequestedOnly)
    }
}

/// The server's `dependencies=true`; leaving it out lists requested packages
/// only.
impl Serialize for DependencySelection {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_bool(*self == Self::Include)
    }
}

/// Package listing filters and pagination, serialised as the query string.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PackageQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u64>,
    #[serde(skip_serializing_if = "DependencySelection::is_requested_only")]
    pub dependencies: DependencySelection,
}

#[cfg(test)]
mod tests {
    use super::{ApiError, AurCacheClient, BuildQuery, DependencySelection, PackageQuery};
    use aurcache_common::build_state::BuildState;

    /// The web UI answers with HTML and a 200, so the decode is where the
    /// mistake first becomes visible -- and "expected value at line 1 column 1"
    /// sends the reader looking at the wrong thing entirely.
    #[test]
    fn an_html_body_is_reported_as_the_wrong_url() {
        let msg = super::json_decode_context("/userinfo", "<!doctype html><html>...");
        assert!(msg.contains("HTML"), "{msg}");
        assert!(msg.contains("/api"), "{msg}");

        let msg = super::json_decode_context("/userinfo", "{\"unexpected\": true}");
        assert!(!msg.contains("HTML"), "{msg}");
    }

    /// The UI is served by the same host as the API, so a wrong URL need not
    /// redirect anywhere: `/health` answers 200 with `index.html`, where the
    /// API answers with nothing at all.
    #[test]
    fn a_page_is_not_an_api_answer() {
        assert!(super::looks_like_html("\n<!doctype html><html>"));
        assert!(!super::looks_like_html(""));
        assert!(!super::looks_like_html("{}"));
    }

    /// A redirect that only adds a trailing slash -- or the empty `?` reqwest
    /// puts on a URL built with no query parameters -- has not gone anywhere.
    /// Compared as text, that empty query reports every correct URL as the web
    /// UI.
    #[test]
    fn a_trailing_slash_is_the_same_endpoint() {
        let same =
            |a: &str, b: &str| super::same_endpoint(&a.parse().unwrap(), &b.parse().unwrap());
        assert!(same(
            "http://host:8080/api/health",
            "http://host:8080/api/health/"
        ));
        assert!(same(
            "http://host:8080/api/health",
            "http://host:8080/api/health?"
        ));
        assert!(!same(
            "http://host:8080/health",
            "http://host:8080/api/login"
        ));
    }

    /// The base keeps its `/api` prefix whichever way its slashes are written.
    #[test]
    fn paths_join_onto_the_base() {
        let client = AurCacheClient::new("http://localhost:8080/api/".to_string(), None).unwrap();
        assert_eq!(
            client.url("/packages/list").unwrap().as_str(),
            "http://localhost:8080/api/packages/list"
        );
        assert_eq!(client.base_url(), "http://localhost:8080/api");
    }

    /// The query strings the server's routes take: absent fields left out,
    /// states as comma-separated keys, and the pkgbase kept for the path.
    #[test]
    fn list_queries_serialise_as_the_routes_read_them() {
        let builds = BuildQuery {
            pkgbase: Some("aewm++".to_string()),
            worker: Some(3),
            states: vec![BuildState::Active, BuildState::Failed],
            limit: Some(10),
            page: None,
        };
        assert_eq!(
            serde_urlencoded::to_string(&builds).unwrap(),
            "worker=3&status=active%2Cfailed&limit=10"
        );
        assert_eq!(
            serde_urlencoded::to_string(BuildQuery::default()).unwrap(),
            ""
        );
        let packages = PackageQuery {
            limit: None,
            page: Some(2),
            dependencies: DependencySelection::Include,
        };
        assert_eq!(
            serde_urlencoded::to_string(&packages).unwrap(),
            "page=2&dependencies=true"
        );
        assert_eq!(
            serde_urlencoded::to_string(PackageQuery::default()).unwrap(),
            ""
        );
    }

    /// Only a 401 counts as a refused session: a 403 means the session was fine
    /// and the request itself was out of bounds, which the on_unauthorized
    /// callback must not misread as "the user needs to log in again".
    #[test]
    fn only_a_401_marks_the_session_refused() {
        let unauthorized: anyhow::Error = ApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            message: "session refused".to_string(),
        }
        .into();
        assert!(super::is_unauthorized(&unauthorized));

        for status in [
            reqwest::StatusCode::FORBIDDEN,
            reqwest::StatusCode::NOT_FOUND,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let error: anyhow::Error = ApiError {
                status,
                message: "something else".to_string(),
            }
            .into();
            assert!(
                !super::is_unauthorized(&error),
                "{status} should not count as refused"
            );
        }
    }
}
