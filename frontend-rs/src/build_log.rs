//! Loading and following a bounded build log, independent of its controls.
use crate::log_tail::{LogCursor, WindowPosition, coarse_pointer, log_tail_cap};
use aurcache_client::{AurCacheClient, Build, BuildRef};
use aurcache_common::api::builds::{DiskUsage, KeptBuild};
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;

const POLL_INTERVAL_MS: u32 = 3_000;
#[derive(PartialEq, Eq)]
enum PageProgress {
    CaughtUp,
    More,
    Retry,
}

#[derive(Clone, Copy)]
pub struct BuildLogController {
    pub cap: usize,
    pub log: Signal<String>,
    pub log_size: Signal<Option<i64>>,
    pub tail: Signal<bool>,
    pub finished: Signal<bool>,
    pub status: Signal<Option<BuildState>>,
    pub worker_name: Signal<Option<String>>,
    pub start_time: Signal<Option<i64>>,
    pub end_time: Signal<Option<i64>>,
    pub disk_usage: Signal<Option<DiskUsage>>,
    pub kept: Signal<Option<KeptBuild>>,
    pub error: Signal<Option<String>>,
    pub following: Signal<bool>,
}

/// Own polling and log state for the mounted build identity.
pub fn use_build_log(identity: BuildRef) -> BuildLogController {
    let cap = use_hook(|| {
        log_tail_cap(
            web_sys::window()
                .and_then(|window| window.inner_width().ok())
                .and_then(|width| width.as_f64()),
            coarse_pointer(),
        )
    });
    let controller = BuildLogController {
        cap,
        log: use_signal(String::new),
        log_size: use_signal(|| None),
        tail: use_signal(|| false),
        finished: use_signal(|| false),
        status: use_signal(|| None),
        worker_name: use_signal(|| None),
        start_time: use_signal(|| None),
        end_time: use_signal(|| None),
        disk_usage: use_signal(|| None),
        kept: use_signal(|| None),
        error: use_signal(|| None),
        following: use_signal(|| true),
    };
    use_future(move || poll_build_log(controller, identity.clone(), cap));
    controller
}

/// Fetch the header and bounded output until a terminal build's final page arrives.
async fn poll_build_log(mut controller: BuildLogController, identity: BuildRef, cap: usize) {
    let client = match crate::api::client() {
        Ok(client) => client,
        Err(error) => {
            controller.error.set(Some(error));
            return;
        }
    };
    let mut cursor = LogCursor::default();
    loop {
        if !crate::poll::hidden() {
            match client.get_build(&identity.pkgbase, identity.number).await {
                Ok(build) => {
                    controller.update_header(&build);
                    let settled = !build.status.is_in_progress();
                    if ((controller.following)() || settled)
                        && controller
                            .fetch_page(&client, &identity, &build, &mut cursor, cap)
                            .await
                            == PageProgress::CaughtUp
                        && settled
                    {
                        controller.finished.set(true);
                        return;
                    }
                }
                Err(error) => controller.error.set(Some(error.to_string())),
            }
        }
        gloo_timers::future::TimeoutFuture::new(POLL_INTERVAL_MS).await;
    }
}

impl BuildLogController {
    /// Apply metadata from a successful detail fetch.
    fn update_header(&mut self, build: &Build) {
        self.worker_name.set(build.worker_name.clone());
        self.status.set(Some(build.status));
        self.start_time.set(build.start_time);
        self.end_time.set(build.end_time);
        self.disk_usage.set(build.disk_usage);
        self.kept.set(build.kept.clone());
        self.log_size.set(build.log_size);
    }

    /// Fetch and append the next page; return whether the recorded end was reached.
    async fn fetch_page(
        &mut self,
        client: &AurCacheClient,
        identity: &BuildRef,
        build: &Build,
        cursor: &mut LogCursor,
        cap: usize,
    ) -> PageProgress {
        let size = build.log_size.and_then(|size| u64::try_from(size).ok());
        if let WindowPosition::Restart { offset } = cursor.position(size, cap as u64) {
            self.log.set(String::new());
            self.tail.set(offset > 0);
        }
        let requested = cursor.next_offset;
        match client
            .build_output_page(
                &identity.pkgbase,
                identity.number,
                Some(cursor.next_offset),
                Some(cap as u64),
            )
            .await
        {
            Ok(page) => {
                self.error.set(None);
                let change = self
                    .log
                    .with_mut(|text| cursor.append_page(text, &page, cap));
                if change.is_tail {
                    self.tail.set(true);
                }
                // Without a recorded size, keep paging until EOF. A page
                // containing only an incomplete UTF-8 suffix cannot advance.
                let reached_end = size
                    .map_or(page.is_empty() || cursor.next_offset == requested, |size| {
                        requested.saturating_add(page.len() as u64) >= size
                    });
                if reached_end {
                    PageProgress::CaughtUp
                } else {
                    PageProgress::More
                }
            }
            Err(error) => {
                self.error.set(Some(error.to_string()));
                PageProgress::Retry
            }
        }
    }
}
