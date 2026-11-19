//! Hand one package back: its bytes on stdout.

use super::file::{Opened, OpenedDir, open_dir_in, pump, stdout};
use anyhow::{Context, Result, bail};
use aurcache_build_agent::layout::{PKG_DIR, Work};
use aurcache_common::artifact::is_artifact;
use std::io::Write;

/// Write `<work>/pkg/<name>` to stdout and nothing else, through one fixed
/// buffer. Refuses a name that is not a package's, a `pkg/` that is not a
/// directory of its own (a link to elsewhere, which is not followed), and
/// anything under that name that is not a regular file. A reader that goes
/// away ends it with an error at the next write.
pub fn run(work: &Work, name: &str) -> Result<()> {
    if !is_artifact(name) {
        bail!("{name:?} is not a package name");
    }
    let path = work.pkg().join(name);
    let OpenedDir::Dir(pkg) = open_dir_in(work.root(), PKG_DIR)
        .with_context(|| format!("cannot open {}", work.pkg().display()))?
    else {
        bail!("{} is not a directory", work.pkg().display());
    };
    let Opened::Regular(mut file) = pkg
        .regular(name)
        .with_context(|| format!("cannot open {}", path.display()))?
    else {
        bail!("{name:?} is not a regular file");
    };
    let mut out = stdout();
    pump(&mut file, &mut *out).with_context(|| format!("cannot write {name}"))?;
    out.flush().context("cannot flush stdout")
}
