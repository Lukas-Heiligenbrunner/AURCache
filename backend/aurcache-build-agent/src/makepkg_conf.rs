//! The `makepkg.conf` a build runs with.
//!
//! The image's own, then the job's assignments, then the agent's, which come
//! last and so win: `PKGDEST`, `BUILDDIR`, `SRCDEST` and the parallelism for
//! the build's CPUs.

use crate::layout::{Scratch, Work};
use aurcache_common::parallelism;
use aurcache_common::shell::quote_argument;
use std::path::Path;

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

/// `source /etc/makepkg.conf`, the job's lines, then the agent's.
#[must_use]
pub fn render(rendering: &Rendering<'_>) -> String {
    let mut out = String::from(IMAGE);
    for line in rendering.job.lines() {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&layout(rendering.work, rendering.scratch));
    out.push_str(&parallelism::makepkg_lines(rendering.cpus));
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

/// A path as one shell word.
fn quote(path: &Path) -> String {
    quote_argument(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::{Rendering, render};
    use crate::layout::{Scratch, Work};
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
    fn the_agents_lines_are_last_and_win() {
        let (work, scratch) = (Work::new("/work"), Scratch::new("/var/lib/aurcache-build"));
        let job =
            "PKGDEST=/elsewhere\nMAKEFLAGS=-j64\n: ${BUILDDIR:=/tmp/b}\neval 'SRCDEST=/tmp/s'\n";
        let conf = render(&rendering(job, &work, &scratch));
        assert!(conf.ends_with(
            "PKGDEST=/work/pkg\n\
             BUILDDIR=/var/lib/aurcache-build/build\n\
             SRCDEST=/var/lib/aurcache-build/srcdest\n\
             MAKEFLAGS=\"-j$(nproc)\"\n"
        ));
        let got = evaluate(
            &conf,
            "printf '%s\\n' \"$PKGDEST\" \"$BUILDDIR\" \"$SRCDEST\" \"$MAKEFLAGS\"",
        );
        assert_eq!(
            got,
            [
                "/work/pkg",
                "/var/lib/aurcache-build/build",
                "/var/lib/aurcache-build/srcdest",
                "-j7",
            ]
        );
    }
}
