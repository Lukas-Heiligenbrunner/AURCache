//! The jobs that run on a schedule, and the one place that turns an expression
//! into a [`Schedule`] for them.
//!
//! The syntax itself is [`aurcache_common::schedule`]'s business; what is
//! decided here is what `H` is hashed from. It is seeded per instance -- from
//! the internal CA's fingerprint, created once on first start and kept in the
//! data directory -- and per job, so two servers, or two jobs on one, do not
//! all start in the same minute, and each keeps its minute across restarts.

use aurcache_common::api::settings::SchedulePreview;
use aurcache_common::schedule::{Schedule, ScheduleError, from_seconds_syntax, is_off, job_seed};
use std::sync::LazyLock;

/// A job that runs on a schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    /// Rebuilds out-of-date packages (`AUTO_UPDATE_SCHEDULE`).
    AutoUpdate,
    /// Re-ranks the mirrorlist (`MIRROR_RANK_SCHEDULE`).
    MirrorRanking,
}

impl Job {
    /// The name `H` is hashed with. Fixed: changing it moves the job's runs.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::AutoUpdate => "auto_update",
            Self::MirrorRanking => "mirror_ranking",
        }
    }
}

/// What identifies this instance to `H`: its CA's fingerprint, or nothing when
/// there is no CA to read (a test, or a server that failed to create one --
/// which does not start anyway).
static INSTANCE: LazyLock<String> = LazyLock::new(|| {
    let path = aurcache_ca::cert_path(&aurcache_common::fs::ca_dir());
    std::fs::read_to_string(&path)
        .map_err(anyhow::Error::from)
        .and_then(|pem| aurcache_ca::cert_fingerprint(&pem))
        .unwrap_or_else(|e| {
            tracing::warn!(
                "no instance identity for schedules ({}: {e:#}); H resolves as on any such instance",
                path.display()
            );
            String::new()
        })
});

/// A job's schedule, as the server reads it.
pub struct JobSchedule {
    pub schedule: Schedule,
    /// The five-field schedule `expr` was read as, when it was written in
    /// 0.5.0's seconds-first syntax.
    pub rewritten: Option<String>,
}

/// `expr` as `job`'s schedule on this instance.
///
/// Also reads 0.5.0's seconds-first syntax, as the schedule it means: the
/// upgrade converts a stored schedule, but cannot reach one an instance sets
/// in its environment, and refusing it would stop the job on upgrade. Saving
/// a schedule still takes only the five-field syntax.
///
/// # Errors
/// When `expr` is not a schedule in either syntax.
pub fn schedule(job: Job, expr: &str) -> Result<JobSchedule, ScheduleError> {
    let seed = job_seed(&INSTANCE, job.name());
    match Schedule::parse(expr, seed) {
        Ok(schedule) => Ok(JobSchedule {
            schedule,
            rewritten: None,
        }),
        Err(e) => {
            let Some(rewritten) = from_seconds_syntax(expr) else {
                return Err(e);
            };
            let schedule = Schedule::parse(&rewritten, seed).map_err(|_| e)?;
            Ok(JobSchedule {
                schedule,
                rewritten: Some(rewritten),
            })
        }
    }
}

/// The next `count` runs of `expr` as `job`'s schedule, from the server's
/// current local time.
#[must_use]
pub fn preview(job: Job, expr: &str, count: usize) -> SchedulePreview {
    if is_off(expr) {
        return SchedulePreview::Disabled;
    }
    // Strict, unlike `schedule`: this previews what saving would store, and
    // saving takes only the five-field syntax.
    let schedule = match Schedule::parse(expr, job_seed(&INSTANCE, job.name())) {
        Ok(schedule) => schedule,
        Err(e) => {
            return SchedulePreview::Invalid {
                reason: e.to_string(),
            };
        }
    };
    let mut at = Vec::with_capacity(count);
    let mut now = jiff::Zoned::now();
    while at.len() < count {
        let Some(next) = schedule.next_after(&now) else {
            break;
        };
        at.push(next.timestamp().as_second());
        now = next;
    }
    if at.is_empty() {
        SchedulePreview::Never
    } else {
        SchedulePreview::Runs { at }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_says_what_kind_of_answer_it_is() {
        assert_eq!(preview(Job::AutoUpdate, "  ", 2), SchedulePreview::Disabled);
        assert_eq!(
            preview(Job::AutoUpdate, "@never", 2),
            SchedulePreview::Disabled
        );
        assert_eq!(
            preview(Job::AutoUpdate, "0 0 31 2 *", 2),
            SchedulePreview::Never
        );
        assert!(matches!(
            preview(Job::AutoUpdate, "0 0 2 * * 1", 2),
            SchedulePreview::Invalid { reason } if reason.contains("0 2 * * 0")
        ));
        let SchedulePreview::Runs { at } = preview(Job::AutoUpdate, "H * * * *", 2) else {
            panic!("an hourly schedule runs");
        };
        assert_eq!(at.len(), 2);
        assert_eq!(at[1] - at[0], 3600);
    }

    /// Two jobs written alike land apart, which is the point of `H`.
    /// 0.5.0's syntax is read as what it meant, and says so.
    #[test]
    fn the_seconds_first_syntax_is_read_as_its_five_field_schedule() {
        let read = schedule(Job::AutoUpdate, "0 0 * * * *").unwrap();
        assert_eq!(read.rewritten.as_deref(), Some("0 * * * *"));
        assert!(
            schedule(Job::AutoUpdate, "0 * * * *")
                .unwrap()
                .rewritten
                .is_none()
        );
        assert!(matches!(
            preview(Job::AutoUpdate, "0 0 * * * *", 1),
            SchedulePreview::Invalid { .. }
        ));
    }

    #[test]
    fn jobs_hash_apart() {
        assert_ne!(
            schedule(Job::AutoUpdate, "H H * * *").unwrap().schedule,
            schedule(Job::MirrorRanking, "H H * * *").unwrap().schedule
        );
    }
}
