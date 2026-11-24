# A Repository page

Status: **Proposed** · Last updated: 2026-11-24

The last step of setting AURCache up is pointing pacman at it. Today only
the CLI does that for you: `aurcli repo config` prints the `pacman.conf`
stanza for the instance, and the steps to trust the signing key. Someone who
uses only the web UI gets the documentation's stanza, with `<server_ip>` to
fill in, and the signing commands on the Settings page. Nothing in the UI
says how big the repository is or what it holds.

This adds a **Repository** entry to the sidebar, leading to a page about the
repository: how to add it to pacman (with a copy button), signing, and what
the repository contains. The address in the stanza is `AURCACHE_PUBLIC_URL`,
which already is the address that clients use; the docs that describe it
otherwise are corrected.

---

## Where things stand

- **`aurcli repo config`** (`aurcache-cli/src/repo.rs`) asks
  `GET /repo/info` for `RepoInfo { name, public_url, listen_port }`, and
  `GET /repo/signing` (no authentication) whether the repository is signed.
  It prints `[<name>]`, a `SigLevel` (`Required` once signing is complete),
  a `Server =` line, and the key trust commands (`KeyTrustCommands`, in
  `aurcache_common::repo`). For the host, it keeps a published host that is
  not loopback, and otherwise uses the host that it reached the API on.
- **`AURCACHE_PUBLIC_URL` is the address for clients, and workers' default.**
  `compose/docker-compose.yaml` sets it to the host's published port, and
  gives its workers `AURCACHE_REPO_URL=http://aurcache:8081`, because "the
  public URL above has the host's port, which nothing listens on in here".
  A worker takes the scheme, port and path of `AURCACHE_PUBLIC_URL` with the
  host that it reached the server on, unless its own `AURCACHE_REPO_URL`
  (or `AURCACHE_REPO_HOST`) says otherwise
  (`aurcache-worker-core/src/repo.rs`).
  `setup/native.md`, `examples/truenas.md`, `examples/raspberry-pi.md` and
  `setup/kubernetes.md` all set `AURCACHE_PUBLIC_URL` to how clients reach
  the repository.
- **One page says otherwise.** `environment-variables.md` describes
  `AURCACHE_PUBLIC_URL` as the "Base URL workers use for the pacman repo",
  with `http://aurcache:8081` as its example, a name that only resolves
  inside a compose network. Followed, it makes the CLI's stanza point
  clients at that name.
- **`aurcli setup compose`** (`aurcache-cli/src/compose.rs`) writes
  `AURCACHE_PUBLIC_URL` but no `AURCACHE_REPO_URL` for its worker. With the
  repository published on a port other than 8081 and `AURCACHE_PUBLIC_URL`
  set to it, that worker would look for the repository on that port inside
  the compose network, where nothing listens on it.
- **Signing in the UI.** The Settings page has a *Repository signing*
  section (`RepositorySigningSection`, `frontend-rs/src/screens/settings.rs`):
  the fingerprint, whether the backfill is done, the trust commands, and a
  link to the key.
- **Size.** `ListStats::repo_size` is the total size of the published
  artifacts, and the dashboard shows it. Nothing breaks it down by
  architecture. `repository-download-stats.md` (proposed) would add
  downloads and traffic.
- **Clipboard.** `crate::clipboard::copy_text` returns
  `CopyError::Unavailable` outside a secure context. Plain http on a LAN
  address, the usual way to reach a home instance, is not a secure context.

## Design

### The address for clients

The stanza is built as `aurcli repo config` builds it today: scheme, port
and path from `AURCACHE_PUBLIC_URL`, and its host unless that is loopback,
in which case the client's host (`location.hostname` on the page, the API
URL's host in the CLI). Nothing new is configured: a deployment sets
`AURCACHE_PUBLIC_URL` to how clients reach the repository, as the shipped
compose file and the setup guides already do, and a worker that reaches it
another way sets `AURCACHE_REPO_URL`.

When `AURCACHE_PUBLIC_URL` is unset or loopback, the page says "This is a
guess. If clients reach the repository at another address, set
`AURCACHE_PUBLIC_URL` on the server", with a link to the docs.

### The page

`Route::Repository`, at `/repository`, with a `Repository` entry in the
sidebar's *General* group after *Workers*. It is a section that you look
at, like the others in that group, unlike the *Add package* button.

```
Repository  repo
┌ Add to pacman ───────────────────────────────────────────────┐
│ Append to /etc/pacman.conf on each machine:          [Copy] │
│ ┌──────────────────────────────────────────────────────────┐ │
│ │ [repo]                                                   │ │
│ │ SigLevel = Required                                      │ │
│ │ Server = http://nas.lan:30081/$arch                      │ │
│ └──────────────────────────────────────────────────────────┘ │
│ Trust the signing key first:                         [Copy] │
│ │ curl -fsSL http://nas.lan:8080/api/repo/key | sudo pac... │ │
│ │ sudo pacman-key --lsign-key 0123…CDEF                    │ │
│ Then run  sudo pacman -Syu                                   │
│                                                             │
│ With aurcli: aurcli repo config --install                    │
└─────────────────────────────────────────────────────────────┘
┌ Contents ───────────────────────────────────────────────────┐
│ 214 packages (37 requested, 177 dependencies)               │
│ x86_64   206 packages   4.1 GiB                             │
│ aarch64   58 packages   1.2 GiB                             │
│ Last published: 2 hours ago                                 │
└─────────────────────────────────────────────────────────────┘
┌ Signing ────────────────────────────────────────────────────┐
│ fingerprint, backfill state, link to the key                │
└─────────────────────────────────────────────────────────────┘
```

- **Add to pacman.** The stanza and trust commands that `aurcli repo config`
  prints, computed by the same code (below). Each block is always shown as
  text, so it can be copied by hand where the button cannot copy. The
  `SigLevel` and the trust commands follow `/repo/signing` as in the CLI:
  `Optional TrustAll` and no commands when unsigned, and a note to keep
  signatures optional while backfilling.
- **Contents.** From `GET /stats`, whose `ListStats` gains per-architecture
  counts and sizes and the time of the last publish: package count
  (requested and dependencies), and per architecture (per target, with
  `build-targets.md`) the number of packages listed and their total size,
  plus when the repository was last published to. Sizes that are not known
  are a dash, and a total over unknown sizes is a dash, as everywhere else.
  With several repositories, each repository's contents come from
  `GET /repositories` instead.
- **Signing** moves here from Settings, unchanged. Settings keeps a one-line
  pointer to it.
- **Downloads**, once `repository-download-stats.md` lands: syncs and
  downloads over the last 30 days, and the most downloaded packages, as a
  card on this page.

With `multiple-repositories.md`, the page shows one set of these cards per
repository, the sidebar entry becomes *Repositories*, and
`AURCACHE_PUBLIC_URL` becomes the base that every repository's path is
added to. With `build-targets.md`, *Add to pacman* offers each published
target, with the target's directory in place of `$arch`.

### One function computes the stanza

The stanza moves to `aurcache_common::repo`, so the page and the CLI cannot
print different lines: `is_loopback` (the CLI's copy in `url.rs`, which
lists `[::1]` and which `setup.rs` also uses; it replaces
the Workers screen's own), `DEFAULT_SIGLEVEL`, `SIGNED_SIGLEVEL`,
the choice of `Server =` line, and a `Stanza { name, siglevel, server }` with
a `block()` method. The functions return `Option`, because
`aurcache-common` does not depend on `anyhow` and the only failure is "no
host in this URL". The CLI's `RepoConfig`, which `--format json` serializes,
is built from `Stanza` and keeps its fields. The tests for choosing the host
move along with the code.

### Copying where the clipboard is unavailable

The copy button is a `CopyBlock { text }` widget in `widgets.rs`. On
`CopyError::Unavailable` it selects the block's text
(`Selection::select_all_children`) and says "Press Ctrl+C to copy". On
`Refused` it shows the error. The Workers screen's `docker run` line could
use the widget later.

### Docs

`Configuration/pacman-repo.md` starts with the Repository page, then the
CLI. In `environment-variables.md`, the row for `AURCACHE_PUBLIC_URL` says
what the rest of the docs already do: the base URL that clients reach the
repository at, such as `http://192.168.1.10:8081`, which workers use too
unless their `AURCACHE_REPO_URL` says otherwise, with a link to the worker
variables in `workers/split-chroot.md`. The note further down the page,
on what replaced 0.5's settings, drops `AURCACHE_REPO_URL`, which 0.5
never had.

Three workers next to the server reach the repository by the server's own
port, and get `AURCACHE_REPO_URL` for it, as `compose/docker-compose.yaml`
already gives its workers. Then setting `AURCACHE_PUBLIC_URL` to a
published port, a path or a reverse proxy does not break them:

- the worker that `aurcli setup compose` writes, with
  `http://aurcache:8081`;
- the local worker that `aurcli setup worker` starts beside a server
  (`beside_server` in `setup.rs`);
- the hybrid image's embedded worker, whose entrypoint defaults it to
  `http://localhost:${AURCACHE_REPO_PORT:-8081}`.

## What this does not do

- **Edit `pacman.conf`.** A browser cannot write files on the machine it
  runs on, and the server is usually not the machine being set up.
- **Find the address by itself.** The server could compare the `Host`
  header of requests to the repository, but that is still how one client
  reached it, and the page already knows that.

## Open questions

- **`-Sy` or `-Syu`.** The CLI and the docs say `sudo pacman -Sy`. Syncing
  without upgrading makes a following `pacman -S` a partial upgrade. The
  page would say `-Syu`. Should the CLI and the docs change to match?
- **Sidebar label.** *Repository*, or *Pacman repository* for someone who
  does not know what the word refers to here?
- **Editing the address in the UI.** Is a redeploy to change
  `AURCACHE_PUBLIC_URL` on TrueNAS enough of a burden to justify a setting
  that overrides it?

## Implementation order

1. Move the stanza into `aurcache_common::repo`. The CLI and the Workers
   screen use it. The CLI's output does not change, and the Workers screen
   also treats `[::1]` as loopback.
2. The environment variables row, and `AURCACHE_REPO_URL` for the three
   workers above. Each is a small fix of its own.
3. The `ListStats` additions, the route, the sidebar entry, `CopyBlock`, the
   signing section moved over, and the docs.

## Verification

- Unit tests in `aurcache-common` for the stanza: a loopback
  `AURCACHE_PUBLIC_URL` (the host comes from the client), a real host
  (kept), and a `public_url` with a path.
- Host-rendered tests of the *Add to pacman* card: unsigned, signed,
  backfilling, and guessed (the note shown).
- `test-frontend.sh` mounts the new route. Its server runs with signing on,
  so the signing card is rendered in its widest form. A screenshot of the
  page goes in the PR.
- By hand: copying over plain http to a LAN address selects the text and
  says so, and over https or `localhost` the button copies.

## Alternatives

- **A dialog from the sidebar**, like *Preferences*. It is about half the UI
  work, but only fits the stanza. A page has room for the contents, signing
  and downloads, can be linked from the docs, and is covered by the
  route-mount test.
- **A section on the Settings page.** It would sit among settings that
  change the server, which this mostly does not do, and the people who need
  it most are not administering the instance.
- **A separate client address setting**, editable on the page. It would
  save a TrueNAS user a redeploy when the address changes, at the cost of a
  setting, a `RepoInfo` field and a second answer to "where is the
  repository". `AURCACHE_PUBLIC_URL` already is that address.
- **Never take the host from `AURCACHE_PUBLIC_URL`.** It would hide a wrong
  `aurcache:8081` example, but break the stanza on every deployment that
  the docs tell to set it, Kubernetes included.
- **Have the server render the stanza.** It cannot when
  `AURCACHE_PUBLIC_URL` is loopback, because the host then depends on how
  the client reaches the server.
