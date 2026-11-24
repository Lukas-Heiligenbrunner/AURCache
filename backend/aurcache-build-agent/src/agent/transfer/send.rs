//! Hand one package back: its bytes on stdout.

use aurcache_build_agent::layout::{PKG_DIR, Work};
use aurcache_common::beneath::Dir;
use std::io::{self, Write};

/// Write the regular file `<work>/pkg/<name>` to stdout and nothing else. A
/// name that is not one name in `pkg/` is refused, and so is anything that
/// is not a regular file there.
pub fn run(work: &Work, name: &str) -> io::Result<()> {
    let mut file = Dir::open(work.root())?.dir(PKG_DIR)?.file(name)?;
    let mut out = super::stdout();
    io::copy(&mut file, &mut *out)?;
    out.flush()
}
