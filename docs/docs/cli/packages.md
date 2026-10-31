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
aurcli pkg rm hello                     # drop the direct-request flag (alias: delete)
```

Adding a package resolves its AUR `depends` and `make_depends`, inserts
dependency links, and enqueues builds leaf-first. `pkg add --from-installed`
prints what it found and asks before submitting; pass `--yes` to skip the
prompt in a script. `pkg rm` removes the direct-request flag — full removal
goes through package deletion on the server.

Dependencies can be repointed when upstream changes what provides them:

```bash
aurcli pkg deps list --package hello --dep libfoo
aurcli pkg deps set --package hello --dep libfoo --to libfoobar
aurcli pkg deps drop --package hello --dep libfoo   # only what the official repos now publish
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
