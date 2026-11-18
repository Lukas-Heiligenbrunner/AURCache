//! The `makepkg.conf` a build runs with.
//!
//! The image's own, then the job's assignments without the lines that say
//! where things go or how many jobs run, then the agent's: `PKGDEST`,
//! `BUILDDIR`, `SRCDEST` and the parallelism for the build's CPUs.

use crate::layout::{Scratch, Work};
use aurcache_common::shell::quote_argument;
use std::path::Path;

/// The variables that are the agent's to set: a job's line assigning one is
/// dropped.
const OWNED: [&str; 4] = ["PKGDEST", "BUILDDIR", "SRCDEST", "MAKEFLAGS"];

/// The image's configuration, as makepkg itself would have loaded it had it
/// not been given a file of its own: the main file, then its drop-ins.
const IMAGE: &str = "\
source /etc/makepkg.conf
for conf in /etc/makepkg.conf.d/*.conf; do
    [[ -r $conf ]] && source \"$conf\"
done
unset conf
";

/// Everything the rendering depends on.
pub struct Rendering<'a> {
    /// The job's makepkg assignments.
    pub job: &'a str,
    pub work: &'a Work,
    pub scratch: &'a Scratch,
    pub cpus: Option<f64>,
}

/// `source /etc/makepkg.conf`, the job's lines minus the layout ones, then the agent's.
#[must_use]
pub fn render(rendering: &Rendering<'_>) -> String {
    let mut out = String::from(IMAGE);
    for line in rendering.job.lines().filter(|line| !assigns_owned(line)) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&layout(rendering.work, rendering.scratch));
    out.push_str(&parallelism(rendering.cpus));
    out
}

/// Where makepkg puts what it makes and fetches.
fn layout(work: &Work, scratch: &Scratch) -> String {
    format!(
        "PKGDEST={}\nBUILDDIR={}\nSRCDEST={}\n",
        quote(&work.pkg()),
        quote(&scratch.build()),
        quote(&scratch.srcdest()),
    )
}

/// How many jobs a build runs: one per core the machine has when nothing
/// limits it, and as many as the CPUs it is limited to otherwise, since
/// `nproc` counts the machine's cores whatever the limit is.
fn parallelism(cpus: Option<f64>) -> String {
    let Some(cpus) = cpus else {
        return "MAKEFLAGS=\"-j$(nproc)\"\n".to_string();
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let jobs = (cpus.ceil() as u64).max(1);
    format!(
        "MAKEFLAGS=\"-j{jobs}\"\nexport OMP_NUM_THREADS={jobs}\nexport CARGO_BUILD_JOBS={jobs}\n"
    )
}

/// A path as one shell word.
fn quote(path: &Path) -> String {
    quote_argument(&path.to_string_lossy())
}

/// Whether a line is a whole assignment to a variable the agent sets.
///
/// Only a line that is complete on its own is dropped: one that continues on
/// the next, by a backslash or an open quote, is kept, since dropping half an
/// assignment would leave a file bash cannot read. The agent's own
/// assignments come last and win either way.
fn assigns_owned(line: &str) -> bool {
    let line = line.trim_start();
    let line = line.strip_prefix("export").map_or(line, |rest| {
        if rest.starts_with([' ', '\t']) {
            rest.trim_start()
        } else {
            line
        }
    });
    let Some((name, value)) = line.split_once('=') else {
        return false;
    };
    let name = name.strip_suffix('+').unwrap_or(name);
    OWNED.contains(&name) && is_complete(value)
}

/// Whether a value ends on the line it starts on.
fn is_complete(value: &str) -> bool {
    let mut single = false;
    let mut double = false;
    let mut depth = 0u32;
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if !single => escaped = true,
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '(' if !single && !double => depth += 1,
            ')' if !single && !double => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    !single && !double && depth == 0 && !escaped
}

#[cfg(test)]
mod tests {
    use super::{Rendering, render};
    use crate::layout::{Scratch, Work};
    use std::path::Path;
    use std::process::Command;

    fn rendering<'a>(job: &'a str, work: &'a Work, scratch: &'a Scratch) -> Rendering<'a> {
        Rendering {
            job,
            work,
            scratch,
            cpus: None,
        }
    }

    /// What bash makes of a rendered file: the named variables, one per line.
    /// So that the answer does not depend on the machine, `nproc` is fixed,
    /// the image's own files are not read, and the environment is empty.
    fn evaluate(conf: &str, print: &str) -> Vec<String> {
        let script = format!("nproc() {{ echo 7; }}\nsource() {{ :; }}\n{conf}\n{print}");
        let out = Command::new("bash")
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .output()
            .expect("running bash");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn the_images_own_configuration_comes_first() {
        let (work, scratch) = (Work::new("/work"), Scratch::new("/work/scratch"));
        let conf = render(&rendering("PACKAGER='x'\n", &work, &scratch));
        assert!(conf.starts_with("source /etc/makepkg.conf\n"), "{conf}");
        let image = conf.find("/etc/makepkg.conf.d/*.conf").unwrap();
        let job = conf.find("PACKAGER='x'").unwrap();
        let agent = conf.find("PKGDEST=").unwrap();
        assert!(image < job && job < agent, "{conf}");
    }

    #[test]
    fn the_jobs_layout_lines_are_dropped() {
        let (work, scratch) = (Work::new("/work"), Scratch::new("/work/scratch"));
        let job = "PKGDEST=/elsewhere\n\
                   export BUILDDIR=/tmp/b\n\
                   \tSRCDEST='/tmp/s'\n\
                   MAKEFLAGS=\"-j64\"\n\
                   MAKEFLAGS+=\" -l8\"\n\
                   PACKAGER='AURCache <aurcache@localhost>'\n\
                   OPTIONS+=(!debug)\n\
                   PKGEXT='.pkg.tar.xz'\n";
        let conf = render(&rendering(job, &work, &scratch));
        for gone in ["/elsewhere", "/tmp/b", "/tmp/s", "-j64", "-l8"] {
            assert!(!conf.contains(gone), "{gone} was kept:\n{conf}");
        }
        for kept in [
            "PACKAGER='AURCache <aurcache@localhost>'\n",
            "OPTIONS+=(!debug)\n",
            "PKGEXT='.pkg.tar.xz'\n",
        ] {
            assert!(conf.contains(kept), "{kept} was dropped:\n{conf}");
        }
    }

    /// An assignment that runs over several lines is left whole, and the
    /// agent's still wins.
    #[test]
    fn an_assignment_over_several_lines_is_kept_and_overridden() {
        let (work, scratch) = (Work::new("/work"), Scratch::new("/work/scratch"));
        let job = "MAKEFLAGS=\"-j64 \\\n-l8\"\nSRCDEST='/a\nb'\n";
        let conf = render(&rendering(job, &work, &scratch));
        assert!(conf.contains(job), "{conf}");
        let got = evaluate(&conf, "printf '%s\\n' \"$MAKEFLAGS\" \"$SRCDEST\"");
        assert_eq!(got, ["-j7", "/work/scratch/srcdest"]);
    }

    #[test]
    fn the_agents_lines_are_last_and_win() {
        let (work, scratch) = (Work::new("/work"), Scratch::new("/var/lib/aurcache-build"));
        // Not lines the agent can recognise as assignments, so they stay.
        let job = "declare PKGDEST=/elsewhere\n: ${BUILDDIR:=/tmp/b}\neval 'SRCDEST=/tmp/s'\n";
        let conf = render(&rendering(job, &work, &scratch));
        assert!(conf.ends_with(
            "PKGDEST=/work/pkg\n\
             BUILDDIR=/var/lib/aurcache-build/build\n\
             SRCDEST=/var/lib/aurcache-build/srcdest\n\
             MAKEFLAGS=\"-j$(nproc)\"\n"
        ));
        let got = evaluate(
            &conf,
            "printf '%s\\n' \"$PKGDEST\" \"$BUILDDIR\" \"$SRCDEST\"",
        );
        assert_eq!(
            got,
            [
                "/work/pkg",
                "/var/lib/aurcache-build/build",
                "/var/lib/aurcache-build/srcdest",
            ]
        );
    }

    #[test]
    fn parallelism_follows_the_cpu_limit() {
        let (work, scratch) = (Work::new("/work"), Scratch::new("/s"));
        let jobs = |cpus: Option<f64>| {
            let mut rendering = rendering("", &work, &scratch);
            rendering.cpus = cpus;
            evaluate(
                &render(&rendering),
                "printf '%s\\n' \"$MAKEFLAGS\" \"${OMP_NUM_THREADS-unset}\" \"${CARGO_BUILD_JOBS-unset}\"",
            )
        };
        // Without a limit, a job per core, and the other two left alone.
        assert_eq!(jobs(None), ["-j7", "unset", "unset"]);
        // With one, as many jobs as CPUs, rounded up, and never none.
        assert_eq!(jobs(Some(4.0)), ["-j4", "4", "4"]);
        assert_eq!(jobs(Some(2.5)), ["-j3", "3", "3"]);
        assert_eq!(jobs(Some(0.5)), ["-j1", "1", "1"]);
        assert_eq!(jobs(Some(0.0)), ["-j1", "1", "1"]);
        assert_eq!(jobs(Some(-3.0)), ["-j1", "1", "1"]);
    }

    /// Paths reach bash as one value each, whatever is in them: nothing in
    /// one is run or expanded.
    #[test]
    fn a_hostile_path_survives_a_real_shell_as_one_value() {
        for hostile in [
            "/w/a b",
            "/w/evil'; touch /tmp/aurcache-pwned; '",
            "/w/a$(touch /tmp/aurcache-pwned)b",
            "/w/x`id`y",
            "/w/back\\slash",
            "/w/semi;colon&amp",
            "/w/glob*?[x]",
            "/w/quote\"d",
            "/w/paren)(",
        ] {
            let work = Work::new(hostile);
            let scratch = Scratch::new(format!("{hostile}/scratch"));
            let got = evaluate(
                &render(&rendering("", &work, &scratch)),
                "printf '%s\\n' \"$PKGDEST\" \"$BUILDDIR\" \"$SRCDEST\"",
            );
            assert_eq!(
                got,
                [
                    format!("{hostile}/pkg"),
                    format!("{hostile}/scratch/build"),
                    format!("{hostile}/scratch/srcdest"),
                ],
                "{hostile:?}"
            );
        }
        assert!(!Path::new("/tmp/aurcache-pwned").exists());
    }
}
