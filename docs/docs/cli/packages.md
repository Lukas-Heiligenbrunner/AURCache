---
sidebar_position: 3
---

# Packages and builds

## Packages

```bash
aurcli pkg list                        # directly requested packages
aurcli pkg list -q                      # names only, for piping
aurcli pkg get hello
aurcli pkg add hello
aurcli pkg add https://github.com/you/pkg.git --ref v1.2 --platform x86_64
aurcli pkg add --from-installed         # bulk-add what `pacman -Qm` reports, behind a confirmation
aurcli pkg update hello
aurcli pkg patch hello --platform x86_64
aurcli pkg patch hello --build-flag=--nocheck   # the whole list; see Build flags
aurcli pkg patch hello --no-build-flags         # clear them: build as makepkg does by default
aurcli pkg rm hello                     # stop requesting it; delete what nothing needs
```

Adding a package resolves its AUR `depends` and `make_depends`, inserts
dependency links, and enqueues builds leaf-first. `pkg add --from-installed`
prints what it found and asks before submitting; pass `--yes` to skip the
prompt in a script.

`pkg rm` stops requesting a package. The package is then deleted — its builds,
its artifacts and their entries in the repository, its build logs and its source
checkout — together with every dependency of it that no requested package still
needs. A package that a requested package still depends on, directly or through
others, stays as a dependency and only stops being requested.

### Uploading packages built elsewhere

A package that cannot be built where the workers run -- one that runs the
build host out of memory, say -- can be built anywhere else, with
`makechrootpkg` or plain `makepkg`, and published through AURCache:

```bash
aurcli pkg upload gcc13 gcc13-*.pkg.tar.zst gcc13-*.pkg.tar.zst.sig
aurcli pkg upload fonts fonts-1.0-1-any.pkg.tar.zst --platform aarch64
```

The files are one build of the named package: its package files (split
packages included) and, optionally, a detached `.sig` for each. They become a
build of it, published like one a worker built: the previous version leaves
the repository, and builds waiting on this package are released. For an AUR or
git package, every file must be among the packages its source builds, as a
worker's would; for an uploaded (or new) package, every package name must
follow the AUR's rule (lowercase letters, digits and `@._+-`, not starting
with `-` or `.`). Every version is `[epoch:]pkgver-pkgrel` as makepkg writes
it. All of them must be one version, for one platform the package is built for
(`arch = any` packages go with the rest, or to `--platform` when there is
nothing else). One file refused refuses the upload, and the error says which
and why; `aurcli` checks the file names before it sends anything. What the
files hold is checked when the build is published, as for a build here: a
package whose `.PKGINFO` does not agree with its filename -- or, for an
uploaded package, does not name it as its `pkgbase` -- or a `.sig` that is
ASCII-armored rather than a binary detached signature, fails the build, and
its log says why. makepkg's `-debug` packages are left out, as for a build
here: the repository does not carry them.

An upload is a whole build of the package: split packages it leaves out leave
the repository on that platform, and its version replaces the one there even
when it is older.

A package that does not exist yet is created as an *uploaded* package: nothing
here ever builds it, and its next version comes from its next upload. It is
created before its first upload is published, and stays if publishing fails:
an empty package with one failed build, until it is uploaded again or removed
(`aurcli pkg rm`). An
upload to an AUR or git package is a build of it; its next upstream version is
built as usual, and if that build fails, the uploaded version stays in the
repository. A build of the package already queued or running on that platform
has to finish, or be cancelled, first.

A package name is published by one package at a time, on each platform: an
upload carrying a name another package already publishes there is refused,
and so is any build that would publish one, whichever kind of package it is
-- the build fails, and its log names the package that has it. A name a
package only builds, and has not published yet, belongs to whichever package
publishes it first. Of packages that already share a name, every build that
publishes it fails until all but one of them are removed (see
[Upgrading](../overview/upgrading.md#one-package-per-package-name)).

The package page has the same upload, for files small enough to read into the
browser; `aurcli` streams them, so it has no such limit beyond the package's
`max_artifact_size`.

Dependencies can be repointed when upstream changes what provides them:

```bash
aurcli pkg dep options hello libfoo            # what could take over libfoo
aurcli pkg dep replace hello libfoo libfoobar  # depend on libfoobar instead
aurcli pkg dep drop hello libfoo               # only what the official repos now publish
```

Also: `aurcli search <query>` searches the AUR through AURCache,
`aurcli stats` shows dashboard statistics, and `aurcli graph` returns monthly
datapoints.

## Builds

```bash
aurcli builds list --status active,publishing
aurcli builds list --worker freyja --package hello
aurcli builds get hello/3
aurcli builds output hello/3           # paged log output (--offset/--limit in bytes)
aurcli builds retry hello/3
aurcli builds cancel hello/3
aurcli builds delete hello/3
aurcli builds watch                    # follow the queue until it drains
aurcli builds watch --build hello/3
```

`--wait` on a triggering command (`pkg add`, `pkg update`, `builds retry`)
waits for what that trigger queued and exits non-zero if any of them fails —
use it instead of a separate `builds watch`, which cannot see builds that
finish in the gap between the two processes. See [Why is a build not
starting](../workers/managing.md#why-is-a-build-not-starting).
