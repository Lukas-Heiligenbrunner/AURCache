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
