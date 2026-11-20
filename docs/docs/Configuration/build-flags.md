---
sidebar_position: 6
---

# Build flags

A package can be built with some of `makepkg`'s options, chosen on its page,
or with `aurcli pkg patch <pkgbase> --build-flag=<flag>`, repeated for the
whole list. A package with none set builds as `makepkg` does by default.

| Flag              | What it does                                                         |
|-------------------|----------------------------------------------------------------------|
| `--nocheck`       | Skip the check() tests, and the checkdepends they need               |
| `--noprepare`     | Skip prepare(), which patches and sets up the sources                |
| `--holdver`       | Build VCS sources as they were fetched, without pulling newer commits |
| `--ignorearch`    | Build even when the PKGBUILD's arch does not list this architecture  |
| `--nodeps`        | Check and install no dependencies before the build                   |
| `--skipchecksums` | Do not verify the sources against their checksums                    |
| `--skippgpcheck`  | Do not verify the sources' PGP signatures                            |

The server also takes a flag in any spelling `makepkg` takes it in (`-d`,
`-dA`) and stores the long name, and `--skipinteg` as the two checks it
skips, `--skipchecksums` and `--skippgpcheck`.

Every build is given `--noconfirm --noprogressbar --nocolor` by its worker,
so these need not be set. Other options that change nothing in a build --
`--syncdeps`, `--check`, `--clean`, `--cleanbuild`, `--force`, `--rmdeps`,
`--needed`, `--asdeps` and `--nosign` -- are accepted and not stored. A
package's build tree, if it keeps one, is the `PERSISTENT_BUILDDIR`
setting's to decide rather than `--clean`'s or `--cleanbuild`'s.

An option that would leave no package (`--nobuild`, `--geninteg`, …), or that
takes over what the worker sets (`--config`, `--log`, …), is refused, with the
reason.
