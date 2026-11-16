//! Hand one package back: its bytes on stdout.

use super::file::{Opened, open_regular, pump, stdout};
use anyhow::{Context, Result, bail};
use aurcache_build_agent::layout::Work;
use aurcache_common::artifact::is_artifact;
use std::io::Write;

/// Write `<work>/pkg/<name>` to stdout and nothing else, through one fixed
/// buffer. Refuses a name that is not a package's, and anything under that
/// name that is not a regular file. A reader that goes away ends it with an
/// error at the next write.
pub fn run(work: &Work, name: &str) -> Result<()> {
    if !is_artifact(name) {
        bail!("{name:?} is not a package name");
    }
    let path = work.pkg().join(name);
    let Opened::Regular(mut file) =
        open_regular(&path).with_context(|| format!("cannot open {}", path.display()))?
    else {
        bail!("{name:?} is not a regular file");
    };
    let mut out = stdout();
    pump(&mut file, &mut *out).with_context(|| format!("cannot write {name}"))?;
    out.flush().context("cannot flush stdout")
}
