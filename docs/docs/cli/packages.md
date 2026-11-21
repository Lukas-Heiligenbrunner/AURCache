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
aurcli pkg add git@github.com:you/private-pkg.git   # over SSH, with the server's key
aurcli ssh key                          # that key, to add to the repository
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

A package that cannot be built where the workers run can be built anywhere
else, with `makechrootpkg` or `makepkg`, and published as a build of it:

```bash
aurcli pkg upload gcc13 gcc13-*.pkg.tar.zst gcc13-*.pkg.tar.zst.sig
aurcli pkg upload fonts fonts-1.0-1-any.pkg.tar.zst --platform aarch64
```

The package must already be here, as an AUR or git package, and nothing of it
may be queued or running on that platform. The files are one build of it:

- every package is one its source builds, each once, with an optional `.sig`;
- all are one version, for one platform the package is built for
  (`--platform` when every file is `arch = any`);
- a `.PKGINFO` that disagrees with its filename, or an ASCII-armored `.sig`,
  fails the build, and its log says why.

It is published like a worker's build: the previous version leaves the
repository, and so does any split package that the upload does not include.
The package page has the same upload, for files small enough to read into the
browser.

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
