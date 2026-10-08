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
    ExistingPackagePolicy, RestoreAccepted, RestoreEntry, RestoreOutcome, RestoreProgress,
    SecretsPolicy,
};
pub use aurcache_common::api::events::{Event, KINDS, Kind, Segment, kind_label};
pub use aurcache_common::api::info::{ServerInfo, Timezone};
pub use aurcache_common::api::log::{
    BuildRef, EntityRef, LogEntry, LogFilter, LogPage, PackageRef, WorkerRef,
};
pub use aurcache_common::api::operations::{ActiveOperation, kind as operation_kind};
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
// The add and preview requests are the server's own shapes rather than copies:
// they were duplicated here, so a field added to one was silently absent from
// the other.
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
/// The server's own shape rather than a copy of it: this was a duplicate
/// declaration, so a field added to one silently did not exist on the other.
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
    base_url: String,
    /// The base as a parsed URL (with trailing slash), so per-request paths
    /// join onto it instead of rebuilding and re-parsing the string on every
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
        // minutes -- so the dump download and the restore upload override it
        // per request (the same reason the worker protocol client keeps a
        // deadline-free upload client).
        //
        // Set per request in `send`, which is the only form reqwest's wasm
        // client offers; natively the builder sets it too, for the few raw
        // requests (`probe_api`) that do not go through `send`. The browser's
        // fetch has no separate connect phase to bound.
        let builder = reqwest::Client::builder();
        #[cfg(not(target_arch = "wasm32"))]
        let builder = builder
            .connect_timeout(CLIENT_CONNECT_TIMEOUT)
            .timeout(CLIENT_REQUEST_TIMEOUT);
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
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
        &self.base_url
    }

    /// How this instance publishes its pacman repository.
    pub async fn repo_info(&self) -> Result<RepoInfo> {
        self.request_json::<RepoInfo, Value>(Method::GET, "/repo/info", &[], None)
            .await
    }

    /// Calls the health endpoint and returns success when the instance is healthy.
    pub async fn health(&self) -> Result<()> {
        match self.probe_api().await? {
            ApiReachability::Api => Ok(()),
            ApiReachability::NotApi { detail } => Err(anyhow::anyhow!(
                "{} answered, but {detail}: that is the web UI, not the API",
                endpoint_url(&self.base_url, "/health")
            )),
        }
    }

    /// Ask the health endpoint whether this URL is the API, and where the
    /// request ended up if it is not.
    pub async fn probe_api(&self) -> Result<ApiReachability> {
        let requested = endpoint_url(&self.base_url, "/health");
        let response = self
            .send::<Value>(Method::GET, "/health", &[], None)
            .await?;
        let response = self.success_or_notify(response).await?;
        let landed_on = response.url().to_string();
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
        self.request_json::<ServerInfo, Value>(Method::GET, "/version", &[], None)
            .await
    }

    /// Fetches information about the currently authenticated user.
    pub async fn user_info(&self) -> Result<UserInfo> {
        self.request_json::<UserInfo, Value>(Method::GET, "/userinfo", &[], None)
            .await
    }

    /// Fetches aggregate dashboard statistics.
    pub async fn stats(&self) -> Result<ListStats> {
        self.request_json::<ListStats, Value>(Method::GET, "/stats", &[], None)
            .await
    }

    /// Fetches monthly dashboard graph datapoints.
    pub async fn graph(&self) -> Result<Vec<GraphDataPoint>> {
        self.request_json::<Vec<GraphDataPoint>, Value>(Method::GET, "/graph", &[], None)
            .await
    }

    /// Fetches every dashboard card in one response, beside `stats`/`graph`.
    pub async fn dashboard(&self) -> Result<DashboardView> {
        self.request_json::<DashboardView, Value>(Method::GET, "/stats/dashboard", &[], None)
            .await
    }

    /// Searches for packages through the AUR-backed search endpoint.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        self.request_json::<Vec<SearchResult>, Value>(
            Method::GET,
            "/search",
            &[("query".to_string(), query.to_string())],
            None,
        )
        .await
    }

    /// Regenerates the authenticated user's API token.
    pub async fn regenerate_api_token(&self) -> Result<ApiTokenResponse> {
        self.request_json::<ApiTokenResponse, Value>(Method::POST, "/token/regenerate", &[], None)
            .await
    }

    /// List packages using pagination and an explicit dependency selection.
    pub async fn list_packages(&self, query: &PackageQuery) -> Result<Vec<SimplePackage>> {
        let query = Query::default()
            .opt("limit", query.limit)
            .opt("page", query.page)
            .opt(
                "dependencies",
                (query.dependencies == DependencySelection::Include).then_some(true),
            );
        self.request_json::<Vec<SimplePackage>, Value>(
            Method::GET,
            "/packages/list",
            query.pairs(),
            None,
        )
        .await
    }

    /// Fetches the details of one package, by pkgbase.
    pub async fn get_package(&self, pkgbase: &str) -> Result<ExtendedPackage> {
        self.request_json::<ExtendedPackage, Value>(
            Method::GET,
            &format!("/package/{pkgbase}"),
            &[],
            None,
        )
        .await
    }

    /// Adds a package using the supplied request payload.
    pub async fn add_package(&self, body: &AddPackageRequest) -> Result<()> {
        self.request_empty(Method::POST, "/package", &[], Some(body))
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
        self.request_json(Method::POST, "/packages", &[], Some(body))
            .await
    }

    /// Reads a bulk add's progress, returning only the entries after the first
    /// `after` of them.
    ///
    /// Pass the number already held so each poll returns just what is new;
    /// pass zero to get the run from the beginning, including whatever happened
    /// before this caller started watching.
    pub async fn bulk_add_progress(&self, job_id: i32, after: usize) -> Result<BulkAddProgress> {
        self.request_json::<BulkAddProgress, ()>(
            Method::GET,
            &format!("/packages/bulk/{job_id}"),
            &[("after".to_string(), after.to_string())],
            None,
        )
        .await
    }

    /// Triggers an update check for the given package.
    ///
    /// Returns the build number, within that package, of each build queued as
    /// a result of the update — one per platform that got one.
    pub async fn update_package(&self, pkgbase: &str, body: &UpdatePackage) -> Result<Vec<i32>> {
        self.request_json(
            Method::POST,
            &format!("/package/{pkgbase}/update"),
            &[],
            Some(body),
        )
        .await
    }

    /// Partially updates package metadata.
    pub async fn patch_package(&self, pkgbase: &str, body: &PackagePatch) -> Result<()> {
        self.request_empty(
            Method::PATCH,
            &format!("/package/{pkgbase}"),
            &[],
            Some(body),
        )
        .await
    }

    /// What could take over one of a package's dependencies.
    pub async fn dependency_options(
        &self,
        pkgbase: &str,
        dependency: &str,
    ) -> Result<DependencyOptions> {
        self.request_json::<DependencyOptions, Value>(
            Method::GET,
            &format!("/package/{pkgbase}/dependency/{dependency}/options"),
            &[],
            None,
        )
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
        self.request_empty(
            Method::PUT,
            &format!("/package/{pkgbase}/dependency/{dependency}"),
            &[],
            Some(&ReplaceDependency {
                replacement: replacement.map(ToString::to_string),
            }),
        )
        .await
    }

    /// Removes the direct-request flag from the given package.
    pub async fn delete_package(&self, pkgbase: &str) -> Result<()> {
        self.request_empty::<Value>(Method::DELETE, &format!("/package/{pkgbase}"), &[], None)
            .await
    }

    /// Lists builds, newest first, optionally of one package only.
    /// Lists builds, newest first, narrowed by `query`.
    pub async fn list_builds(&self, query: &BuildQuery) -> Result<Vec<Build>> {
        let states = (!query.states.is_empty()).then(|| {
            query
                .states
                .iter()
                .map(|state| state.key())
                .collect::<Vec<_>>()
                .join(",")
        });
        let params = Query::default()
            .opt("limit", query.limit)
            .opt("page", query.page)
            .opt("worker", query.worker)
            .opt("status", states);
        // Builds for one package are a sub-resource, not a query filter: a
        // pkgbase may contain `+`, which is literal in a path but decodes to a
        // space in a query value.
        let path = match &query.pkgbase {
            Some(pkgbase) => format!("/package/{pkgbase}/builds"),
            None => "/builds".to_string(),
        };
        self.request_json::<Vec<Build>, Value>(Method::GET, &path, params.pairs(), None)
            .await
    }

    /// Lists the files in a package's source tree.
    pub async fn list_source_files(&self, pkgbase: &str) -> Result<SourceFileList> {
        self.request_json::<SourceFileList, Value>(
            Method::GET,
            &format!("/package/{pkgbase}/source/files"),
            &[],
            None,
        )
        .await
    }

    /// Reads one source file, pristine and patched.
    pub async fn get_source_file(&self, pkgbase: &str, path: &str) -> Result<SourceFileContent> {
        let query = Query::default().opt("path", Some(path));
        self.request_json::<SourceFileContent, Value>(
            Method::GET,
            &format!("/package/{pkgbase}/source/file"),
            query.pairs(),
            None,
        )
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
        self.request_empty(
            Method::PUT,
            &format!("/package/{pkgbase}/source/file"),
            &[],
            Some(&body),
        )
        .await
    }

    /// Fetch one build by its public identity, `<pkgbase>/<number>`.
    pub async fn get_build(&self, pkgbase: &str, number: i32) -> Result<Build> {
        self.request_json::<Build, Value>(
            Method::GET,
            &format!("/package/{pkgbase}/build/{number}"),
            &[],
            None,
        )
        .await
    }

    /// Long-running operations still in flight.
    ///
    /// A bulk add or restore outlives the page that started it, so this is how
    /// a browser finds one again -- after a reload, or from a tab that never
    /// started it.
    pub async fn active_operations(&self) -> Result<Vec<ActiveOperation>> {
        self.request_json::<Vec<ActiveOperation>, Value>(Method::GET, "/operations", &[], None)
            .await
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
        let query = Query::default().opt("offset", offset).opt("limit", limit);
        self.request_bytes::<Value>(
            Method::GET,
            &format!("/package/{pkgbase}/build/{number}/output"),
            query.pairs(),
            None,
        )
        .await
    }

    /// Re-runs the given build, returning the new build's number within the
    /// same package.
    pub async fn retry_build(&self, pkgbase: &str, number: i32) -> Result<i32> {
        self.request_json::<i32, Value>(
            Method::POST,
            &format!("/package/{pkgbase}/build/{number}/retry"),
            &[],
            None,
        )
        .await
    }

    /// Requests cancellation of the given build.
    pub async fn cancel_build(&self, pkgbase: &str, number: i32) -> Result<()> {
        self.request_empty::<Value>(
            Method::POST,
            &format!("/package/{pkgbase}/build/{number}/cancel"),
            &[],
            None,
        )
        .await
    }

    /// Deletes the given build record.
    pub async fn delete_build(&self, pkgbase: &str, number: i32) -> Result<()> {
        self.request_empty::<Value>(
            Method::DELETE,
            &format!("/package/{pkgbase}/build/{number}"),
            &[],
            None,
        )
        .await
    }

    /// Lists the files of a source that has not been added yet.
    ///
    /// The source travels in the body rather than being named by pkgbase,
    /// because there is no package to name: this is what lets a PKGBUILD be
    /// inspected — and fixed — before the package exists.
    pub async fn preview_source_files(&self, source: &SourceData) -> Result<SourceFileList> {
        self.request_json(
            Method::POST,
            "/package/source/preview/files",
            &[],
            Some(&SourcePreviewRequest {
                source: source.clone(),
            }),
        )
        .await
    }

    /// Reads one pristine file from a source that has not been added yet.
    pub async fn preview_source_file(
        &self,
        source: &SourceData,
        path: &str,
    ) -> Result<SourceFileContent> {
        self.request_json(
            Method::POST,
            "/package/source/preview/file",
            &[],
            Some(&SourcePreviewFileRequest {
                source: source.clone(),
                path: path.to_string(),
            }),
        )
        .await
    }

    /// Fetches every setting with the value in force and where it came from.
    ///
    /// Pass a `pkgbase` for the per-package view, where a setting the package
    /// does not override reports the global value it inherits.
    pub async fn settings(&self, pkgbase: Option<&str>) -> Result<ApplicationSettings> {
        self.request_json::<ApplicationSettings, Value>(
            Method::GET,
            &self.settings_path(pkgbase, None),
            &[],
            None,
        )
        .await
    }

    /// Reads one setting: the value in force, and where it came from.
    ///
    /// Separate from [`Self::settings`] because not every setting is in that
    /// response — the config files are large text blobs, and sending both of
    /// them with every settings fetch would be wasteful.
    pub async fn get_setting(&self, pkgbase: Option<&str>, key: &str) -> Result<SettingResponse> {
        self.request_json::<SettingResponse, Value>(
            Method::GET,
            &self.settings_path(pkgbase, Some(key)),
            &[],
            None,
        )
        .await
    }

    /// Stores a value for one setting, overriding whatever it inherits.
    ///
    /// The value is sent as a string whatever its type: the server owns the
    /// parsing, so a client that formats a number differently cannot store
    /// something the server would reject on read.
    pub async fn patch_setting(&self, pkgbase: Option<&str>, key: &str, value: &str) -> Result<()> {
        self.request_empty(
            Method::PATCH,
            &self.settings_path(pkgbase, Some(key)),
            &[],
            Some(&SettingValue {
                value: value.to_string(),
            }),
        )
        .await
    }

    /// When `value` would run, for a setting that holds a schedule; nothing is
    /// stored. The server answers because only it knows its timezone and what
    /// `H` resolves to on this instance.
    pub async fn preview_schedule(&self, key: &str, value: &str) -> Result<SchedulePreview> {
        self.request_json(
            Method::POST,
            &format!("{}/schedule", self.settings_path(None, Some(key))),
            &[],
            Some(&SettingValue {
                value: value.to_string(),
            }),
        )
        .await
    }

    /// Drops this scope's stored value, so the setting inherits again.
    pub async fn reset_setting(&self, pkgbase: Option<&str>, key: &str) -> Result<()> {
        self.request_empty::<Value>(
            Method::DELETE,
            &self.settings_path(pkgbase, Some(key)),
            &[],
            None,
        )
        .await
    }

    /// Settings are a sub-resource of the package in the per-package scope, so
    /// every one of the calls above has the same two shapes.
    fn settings_path(&self, pkgbase: Option<&str>, key: Option<&str>) -> String {
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
        let query = Query::default()
            .opt("limit", limit)
            .opt("offset", offset)
            .opt("severity", filter.severity.map(|s| s.slug().to_string()))
            .opt("since_boot", filter.since_boot.then_some(true))
            .opt("kind", filter.kind.clone())
            .opt("entity", filter.entity.as_ref().map(ToString::to_string))
            .opt("role", filter.role.clone());
        self.request_json::<LogPage, Value>(Method::GET, "/log", query.pairs(), None)
            .await
    }

    /// Lists all enrolled remote build workers and their status.
    pub async fn list_workers(&self) -> Result<Vec<Worker>> {
        self.request_json::<Vec<Worker>, Value>(Method::GET, "/workers", &[], None)
            .await
    }

    /// What one worker declares it can be configured with, and what it reports
    /// it is running.
    ///
    /// Apart from [`Self::list_workers`] because it is fetched for the one
    /// worker being looked at, while the list is polled for the whole fleet.
    pub async fn worker_config(&self, id: i32) -> Result<WorkerConfigView> {
        self.request_json::<WorkerConfigView, Value>(
            Method::GET,
            &format!("/workers/{id}/config"),
            &[],
            None,
        )
        .await
    }

    /// Saves values for several of a worker's settings as one change, and
    /// returns the configuration as it stands afterwards. `None` removes a
    /// value so the worker falls back to its own.
    pub async fn update_worker_config(
        &self,
        id: i32,
        update: &WorkerConfigUpdate,
    ) -> Result<WorkerConfigView> {
        self.request_json(
            Method::PATCH,
            &format!("/workers/{id}/config"),
            &[],
            Some(update),
        )
        .await
    }

    /// The image and worker-protocol port a new worker's `docker run` needs.
    pub async fn worker_join_info(&self) -> Result<WorkerJoinInfo> {
        self.request_json::<WorkerJoinInfo, Value>(Method::GET, "/workers/join-info", &[], None)
            .await
    }

    /// Approves a pending worker, signing its CSR so it can build.
    pub async fn approve_worker(&self, id: i32) -> Result<()> {
        self.request_empty::<Value>(Method::POST, &format!("/workers/{id}/approve"), &[], None)
            .await
    }

    /// Asks a worker to take no new builds and let the ones it holds finish.
    pub async fn pause_worker(&self, id: i32) -> Result<()> {
        self.request_empty::<Value>(Method::POST, &format!("/workers/{id}/pause"), &[], None)
            .await
    }

    /// Lets a paused worker take builds again.
    pub async fn resume_worker(&self, id: i32) -> Result<()> {
        self.request_empty::<Value>(Method::POST, &format!("/workers/{id}/resume"), &[], None)
            .await
    }

    /// Revokes a worker, immediately refusing its certificate on the next call.
    pub async fn revoke_worker(&self, id: i32) -> Result<()> {
        self.request_empty::<Value>(Method::POST, &format!("/workers/{id}/revoke"), &[], None)
            .await
    }

    /// Sends a request and decodes the response body as JSON.
    ///
    /// This is intended for endpoints that are not yet wrapped by a typed helper.
    pub async fn request_json<T, B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response = self.send(method, path, query, body).await?;
        let response = self.success_or_notify(response).await?;
        Self::decode_json(response, path).await
    }

    /// Decode a success response as JSON.
    ///
    /// Read the body first: `json()` consumes the response, leaving nothing
    /// to explain the failure with beyond serde's "expected value at line 1
    /// column 1", which describes the symptom and not the cause.
    async fn decode_json<T: DeserializeOwned>(response: Response, path: &str) -> Result<T> {
        let body = response
            .text()
            .await
            .with_context(|| format!("failed to read the response from {path}"))?;
        serde_json::from_str(&body).with_context(|| json_decode_context(path, &body))
    }

    /// Sends a request that is expected to return no meaningful response body.
    pub async fn request_empty<B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<()>
    where
        B: Serialize + ?Sized,
    {
        let response = self.send(method, path, query, body).await?;
        self.success_or_notify(response).await?;
        Ok(())
    }

    /// Sends a request and returns the raw response body as text.
    pub async fn request_text<B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<String>
    where
        B: Serialize + ?Sized,
    {
        let response = self.send(method, path, query, body).await?;
        let response = self.success_or_notify(response).await?;
        response
            .text()
            .await
            .with_context(|| format!("failed to read text response from {path}"))
    }

    /// Fetch a binary response body.
    ///
    /// Separate from [`Self::request_text`] because a dump is a `.tar.gz`:
    /// decoding it as UTF-8 would corrupt it.
    pub async fn request_bytes<B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<Vec<u8>>
    where
        B: Serialize + ?Sized,
    {
        // A binary body is a dump, as large as the restore upload, so it gets
        // the upload's deadline rather than the one for API round trips.
        let response = self
            .send_with_timeout(method, path, query, body, Some(CLIENT_UPLOAD_TIMEOUT))
            .await?;
        let response = self.success_or_notify(response).await?;
        Ok(response
            .bytes()
            .await
            .with_context(|| format!("failed to read response body from {path}"))?
            .to_vec())
    }

    /// Restore a dump, returning before it has finished.
    ///
    /// A dry run reports what it *would* do and no job id, because it changed
    /// nothing to watch.
    pub async fn restore(
        &self,
        archive: bytes::Bytes,
        dry_run: bool,
        on_existing: ExistingPackagePolicy,
        clear: bool,
        secrets: SecretsPolicy,
    ) -> Result<RestoreAccepted> {
        let mut url = self.base.join("restore").context("invalid restore URL")?;
        url.query_pairs_mut()
            .append_pair("dry_run", &dry_run.to_string())
            .append_pair("on_existing", on_existing.as_str())
            .append_pair("clear", &clear.to_string())
            .append_pair("secrets", secrets.as_str());
        let response = self
            .client
            .post(url)
            .timeout(CLIENT_UPLOAD_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/gzip")
            .body(archive);
        let response = match &self.token {
            Some(token) => response.bearer_auth(token),
            None => response,
        };
        let response = self
            .success_or_notify(response.send().await.context("request failed")?)
            .await?;
        Self::decode_json(response, "/restore").await
    }

    /// Read a restore's progress, returning only entries after the first
    /// `after` of them.
    pub async fn restore_progress(&self, job_id: i32, after: usize) -> Result<RestoreProgress> {
        self.request_json::<RestoreProgress, ()>(
            Method::GET,
            &format!("/restore/{job_id}"),
            &[("after".to_string(), after.to_string())],
            None,
        )
        .await
    }

    /// Download a lite export of the server's authored state.
    ///
    /// `include_secrets` adds the CA private key, the worker certificates and
    /// the API token hashes. The resulting file can mint worker identities this
    /// server accepts; treat it as a credential.
    pub async fn dump(&self, include_secrets: bool) -> Result<Vec<u8>> {
        self.request_bytes::<()>(
            Method::GET,
            "/dump",
            &[("include_secrets".to_string(), include_secrets.to_string())],
            None,
        )
        .await
    }

    async fn send<B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<Response>
    where
        B: Serialize + ?Sized,
    {
        self.send_with_timeout(method, path, query, body, None)
            .await
    }

    /// [`Self::send`], with the default total deadline replaced by `timeout`
    /// when one is given.
    async fn send_with_timeout<B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
        timeout: Option<std::time::Duration>,
    ) -> Result<Response>
    where
        B: Serialize + ?Sized,
    {
        // Absolute URLs pass through (as `endpoint_url` also allows); relative
        // paths join the parsed base instead of re-parsing a rebuilt string.
        let mut url = if path.starts_with("http://") || path.starts_with("https://") {
            reqwest::Url::parse(path)
        } else {
            self.base.join(path.trim_start_matches('/'))
        }
        .with_context(|| format!("invalid URL for path {path}"))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }

        let mut request = self.client.request(method, url);
        request = request.timeout(timeout.unwrap_or(CLIENT_REQUEST_TIMEOUT));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }

        request.send().await.context("request failed")
    }

    /// Turns a response into an error where it is not a success, and runs the
    /// `on_unauthorized` callback when the failure was the server refusing the
    /// session.
    ///
    /// Every response goes through this rather than the free
    /// [`ensure_success`], so no call can miss the callback.
    async fn success_or_notify(&self, response: Response) -> Result<Response> {
        match ensure_success(response).await {
            Ok(response) => Ok(response),
            Err(error) if is_unauthorized(&error) => {
                if let Some(on_unauthorized) = &self.on_unauthorized {
                    on_unauthorized();
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
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
fn same_endpoint(requested: &str, landed_on: &str) -> bool {
    match (Url::parse(requested), Url::parse(landed_on)) {
        (Ok(a), Ok(b)) => {
            a.scheme() == b.scheme()
                && a.host_str() == b.host_str()
                && a.port_or_known_default() == b.port_or_known_default()
                && a.path().trim_end_matches('/') == b.path().trim_end_matches('/')
        }
        _ => requested.trim_end_matches('/') == landed_on.trim_end_matches('/'),
    }
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

fn endpoint_url(base_url: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else {
        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

/// Which builds [`AurCacheClient::list_builds`] returns. Every field
/// left unset matches everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildQuery {
    /// Builds of this package only.
    pub pkgbase: Option<String>,
    /// Builds this worker (by id) ran, or is running.
    pub worker: Option<i32>,
    /// Builds in one of these states. Empty is every state.
    pub states: Vec<aurcache_common::build_state::BuildState>,
    pub limit: Option<u64>,
    pub page: Option<u64>,
}

/// Accumulates the `?key=value` pairs of a request, skipping absent values.
#[derive(Default)]
struct Query(Vec<(String, String)>);

impl Query {
    /// Append `key=value` if `value` is `Some`.
    fn opt<T: ToString>(mut self, key: &str, value: Option<T>) -> Self {
        if let Some(value) = value {
            self.0.push((key.to_string(), value.to_string()));
        }
        self
    }

    fn pairs(&self) -> &[(String, String)] {
        &self.0
    }
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

/// Package listing filters and pagination.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageQuery {
    pub limit: Option<u64>,
    pub page: Option<u64>,
    pub dependencies: DependencySelection,
}

#[cfg(test)]
mod tests {
    use super::{ApiError, endpoint_url};

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
        assert!(super::same_endpoint(
            "http://host:8080/api/health",
            "http://host:8080/api/health/"
        ));
        assert!(super::same_endpoint(
            "http://host:8080/api/health",
            "http://host:8080/api/health?"
        ));
        assert!(!super::same_endpoint(
            "http://host:8080/health",
            "http://host:8080/api/login"
        ));
    }

    #[test]
    fn endpoint_url_trims_slashes() {
        assert_eq!(
            endpoint_url("http://localhost:8080/api/", "/packages/list"),
            "http://localhost:8080/api/packages/list"
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
