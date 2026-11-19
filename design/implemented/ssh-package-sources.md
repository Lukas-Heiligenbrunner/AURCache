# Design: SSH package sources

Status: **Implemented** · Last updated: 2026-11-16

A package can be added from a git repository, and the server clones that
repository itself, to read the PKGBUILD before anything is built. Until now
that clone only worked over http(s): `git2` was built without an SSH
transport, and the server had no key. A private repository is usually only
reachable over SSH (`git@host:repo.git`, `ssh://git@host:2222/repo.git`), so
private packaging repositories could not be built at all.

Operator-facing documentation: `docs/docs/Configuration/ssh-sources.md`.

## Roles

Two different keys answer to two different remotes, and stay apart:

| Key | Held by | Used for |
|---|---|---|
| The server's (this design) | the server | the package's **own** repository: clone, fetch, `ls-remote` |
| A worker's (`WORKER_GIT_SSH_KEY`, `docs/workers/credentials.md`) | each worker | the sources a PKGBUILD's `source=()` downloads, or its `prepare()` clones, while building |

The server offers its key only through `SnapshotStore`, which is what clones a
package's own source. The VCS check (`vcs_check.rs`), which asks the remotes a
PKGBUILD *names* whether they moved, uses `Access::Anonymous`: those remotes are
written by whoever wrote the PKGBUILD, and the server's key is not theirs to be
offered. An SSH source there is refused before connecting, and reported as
unresolved, as it was before (it then failed for want of a transport).

## Transport: libssh2 through `git2`, not the `git` CLI

`git2`'s `ssh` feature (libssh2) was chosen over shelling out to `git` with
`GIT_SSH_COMMAND`:

- **No new runtime dependency.** The server image is `debian:bookworm-slim`
  with neither `git` nor `openssh-client`; libssh2 is vendored and linked
  statically like the vendored OpenSSL already is. The Arch package links the
  system `libgit2`, which Arch builds against libssh2 already. A libgit2 built
  otherwise -- without SSH, or with its `exec` backend, which runs `ssh` and
  calls none of the callbacks -- is found at start
  (`git_libgit2_feature_backend`, `git::SshBackend`), and SSH sources are then
  refused with that reason in place of the key, rather than cloned past the
  server's key and host-key checks.
- **One code path.** Clone, fetch and `ls-remote` stay `git2` calls for every
  remote; the only difference for SSH is the callbacks. Shelling out would have
  meant a second implementation of the network half for one scheme, with its
  own error handling and its own idea of refs.
- **Host keys are checked by our code**, in the certificate callback, against
  files we read. The failure messages can then name the host, both fingerprints
  and the command that fixes it, which OpenSSH's own messages would not (they
  would also have to be scraped from stderr).

What it costs, and is documented:

- `ssh_config` is not read: no `Host` aliases, `ProxyJump` or `IdentityFile`.
  The URL carries the real host and port.
- libssh2 picks the host-key algorithm (ECDSA before ed25519 before RSA), not
  what `known_hosts` already holds as OpenSSH would. A host recorded only by
  another key type is refused (`HostKeyCheck::OtherTypes`), saying which types
  are recorded and how to record the offered one, not warning of
  interception; the docs say to record every type (`ssh-keyscan` without
  `-t`). Only the same type with another key reads as a changed key.
- No ssh-agent and no hardware keys. A passphrase only from a file (see below).
- libgit2 replaces a callback's error message with a generic one ("invalid or
  unknown remote ssh hostkey"). `git.rs` keeps the callback's reason beside it
  (`Refusal`) and reports that instead.

## The key

`aurcache_utils::ssh`, in precedence order:

1. `AURCACHE_GIT_SSH_KEY` names a key: used as is, never generated or
   overwritten. A missing file is an error at start, not a reason to generate
   one: two keys would make "which one is in use?" ambiguous exactly when an
   authentication failure is being debugged (the worker's rule too).
2. Otherwise an ed25519 key is generated on first start, in Rust (`ssh-key`
   crate; the image has no `ssh-keygen`), at `AURCACHE_SSH_DIR/id_ed25519`
   (`./data/ssh`), written `0600` in a `0700` directory, its `.pub` beside it.

A key that cannot be loaded does not stop the server: everything else works,
and each SSH source fails with the reason (`SshKey::Unavailable`).

Either key may be protected by a passphrase, given in a file named by
`AURCACHE_GIT_SSH_KEY_PASSPHRASE_FILE`. It is read once at start, held in a
`Zeroizing` string with a redacting `Debug`, and passed to libssh2 as the
fourth argument of `Cred::ssh_key`. For an OpenSSH-format key the server checks
at start that it decrypts the key (`ssh-key`'s `encryption` feature), so a
wrong passphrase is the stated reason rather than an authentication failure
later; an older format is left to libssh2. The generated key is written
unprotected: a passphrase the server chose and stored beside it would protect
nothing. The operator can protect it afterwards with `ssh-keygen -p`.

The public half is logged at start, returned by `GET /api/ssh`, shown on the
settings page and in the add dialog once the entry is an SSH remote, and
printed by `aurcli ssh key` (the key alone on the first line).

`SshInfo` holds the key (public key, fingerprint, origin), the host-key policy
and the trusted host keys. One key serves every remote. Keys chosen per
repository (GitHub binds a deploy key to one) would join it as an optional
field, without changing what is there, and `aurcli ssh` is a group so that
commands for them can sit beside `ssh key` and `ssh hosts`.

### Keeping it

`AURCACHE_SSH_DIR` is a state directory like the CA's, with the same handling:

- `HybridDir::Ssh`: in a container whose `./data/ssh` is not on a volume, the
  key lives in `repo/.ssh`, and moves out once a volume appears. A key lost
  with a container would be silently replaced by one no remote knows.
- `private-state.sh` closes it (`0700`) at every container start.
- It is on the startup warning's list of unpersisted directories.

### Keeping it from PKGBUILDs

`pkgbuild::private_state` names `AURCACHE_SSH_DIR`, the key
`AURCACHE_GIT_SSH_KEY` points at and the passphrase file, so a PKGBUILD parse
cannot read any of them. On a kernel without Landlock the parse sandbox checks
file modes instead and refuses
to parse while the key is readable by other users -- a world-readable Docker
secret then stops every parse, with a message saying which file. That is the
sandbox doing its job (the parse user *could* read it), and the docs say to keep
the key `0600`.

The parse sandbox keeps a private path unreadable as it is written and grants
what is beside it, following symlinks. So every private path is also named by
its real path (`pkgbuild::with_real_paths`): a supplied key behind a link (a
Kubernetes secret is `key -> ..data/key`), or a data directory linked
elsewhere, would otherwise stay readable where it really is.

Nothing returns or logs the private key: the API type has no field for it, and
a test asserts neither the JSON nor `Debug` of `SshAccess` contains it.

## Host keys

Never trusted silently. Two policies (`HostKeyPolicy`):

- **Trust on first use** (default). The first connection to a host records its
  key in `AURCACHE_SSH_DIR/known_hosts`, and an `ssh.host_trusted` warning in
  the activity log with the name, key type and fingerprint: the one key nothing
  checked, recorded where someone will see it to check it. A different key
  from that host later is refused.

  Every key the file in use vouches for is listed with its fingerprint in
  `SshInfo::trusted_hosts` (`TrustedHosts`, a list or why the file cannot be
  read), on the settings page and by `aurcli ssh hosts`, so a fingerprint can
  be compared with the one its host publishes at any time, not only in the
  log entry. A hashed entry is listed without a name, since it has none to
  give; `@revoked` and `@cert-authority` lines vouch for nothing and are not
  listed. This is
  OpenSSH's `StrictHostKeyChecking=accept-new`, which is also what workers do
  without `WORKER_SSH_KNOWN_HOSTS`, so the two behave alike.
- **Pinned**: `AURCACHE_SSH_KNOWN_HOSTS` names an operator's file. Only its
  hosts are trusted, it is never written to, and an unknown host is refused
  with the `ssh-keyscan` line that would add it.

Hosts are named as OpenSSH names them (`host`, or `[host]:port` off port 22),
from the URL rather than from libgit2. The certificate callback is told the
host but not the port, so the URL has to be read exactly as libgit2 reads it,
or a key could be judged under one port's name while another is connected to.
`aurcache_common::source` reads it as libgit2 1.9 does, untrimmed: an
`ssh://` authority from its end, and the scp-like form as `user@host:path`,
which carries no port. libgit2 also takes brackets around the account and
host, inside which a port may follow (`[git@host:2222]:repo` connects to port
2222). Reading that form exactly would take a copy of
`git_net_url_parse_scp`'s state machine, every corner of which decides a
port; instead any bracket in the account or host is `Transport::BracketedScp`,
refused before connecting with the `ssh://` spelling of the same remote. That
includes an IPv6 literal's (`git@[::1]:repo`): libgit2 1.9 keeps the brackets
in the host it resolves, and fails to. An IPv6 host is written
`ssh://git@[::1]/repo`. The callback still refuses a host other than the
one parsed, which catches what a divergence in the host alone would cause.

libgit2 also decides *whether* a URL is reached over SSH: by prefix, ignoring
case, and for anything else with a colon in it, including `./a:b` and schemes
it has no transport for. `source::transport` makes the same decision. What it
would reach over SSH with no host this can name (`Transport::UnreadableSsh`)
is refused before connecting, with either `Access`: without our callbacks,
libgit2 would connect checking only its own idea of `~/.ssh/known_hosts`.

All of that is decided on the URL as given, but libgit2 applies git config's
`url.<base>.insteadOf` (the repository's, the user's, the system's) when it
creates or loads a remote, and connects to the result. `git.rs` refuses a
remote whose effective URL is not the one routed, before connecting, rather
than routing a second time: a rewrite on a server is a misconfiguration to
report, and one URL judged is simpler than two.

The file is parsed with `ssh-key`;
hashed names (`|1|salt|hash`, HMAC-SHA1), comma lists, `*`/`?` globs and `!`
negations match as in OpenSSH. `@revoked` refuses the key wherever it appears;
`@cert-authority` lines are ignored, since certificates are not checked. A line
that does not parse is skipped with a warning, as OpenSSH skips it.

### Rejected: built-in keys for GitHub, GitLab, Codeberg

Pinning the big forges' published keys in the binary would remove first-use
trust for the common case. It was left out: those keys rotate (GitHub's RSA key
did in 2023), and a release carrying a stale key fails closed for every
deployment until it is upgraded. An operator who wants pinning has
`AURCACHE_SSH_KNOWN_HOSTS`, filled by one `ssh-keyscan`.

## Dumps

The rule: a dump without secrets has nothing of SSH in it, so it restores
anywhere and needs no key. A dump with secrets carries what keeps private
sources cloning after a restore or a migration, so that no key has to be added
to every repository again. `SshAccess::export`, `plan_restore` and
`apply_restore` are the whole of it, and the dump and restore code only call
them; the state is `DumpSecrets::ssh` (`DumpSsh`).

- **Carried, with secrets only:** the key the server uses, generated or
  supplied, read from its file and written to `ssh/key` byte for byte (an
  encrypted key stays encrypted), its public half as `ssh/key.pub`, and the
  trust-on-first-use `known_hosts` as `ssh/known_hosts`. Like the CA, the key
  is read from the file, never from what is loaded. The passphrase is never
  carried, and a pinned `AURCACHE_SSH_KNOWN_HOSTS` is the operator's file, so it
  is not carried either.
- **Restore** follows the policy shared with the signing key's restore:
  - *Validate first, and only what will be written.* `SshAccess::plan_restore`
    changes nothing (it writes only a probe file, which it removes). It checks
    that the files it would write can be written (`probe_writable`), that the
    dump's trusted hosts do not contradict this server's, and that the dump's
    key is fit for where it goes:
    - as the key in use, it must open here (a protected key needs this
      server's passphrase file, and is decrypted to prove it);
    - beside an operator's key, unused, it is checked against this server's
      passphrase when there is one, and not made to need one when there is
      not;
    - where the operator's key file is the server's own, it is not written,
      so nothing about it is demanded.

    An unprotected dump key written on a server with a passphrase file is
    protected with that passphrase first, so a server that keeps its key
    protected does not end up with one in the clear. Any failure refuses the
    whole restore before pass 1 ("nothing was imported: …"); the dry run
    reports the same. `apply_restore` then runs after the rows, and the loaded
    key, behind a lock, is reloaded. "Installed" is reported only for a key
    that was checked to open.
  - *No secrets, no change.* Without `--secrets copy` and without `--clear`,
    nothing is touched.
  - *`--secrets copy`:* the key goes to the server's own key file
    (`AURCACHE_SSH_DIR/id_ed25519`, whatever its type); `known_hosts` is merged,
    never replaced, so a restore of an older dump cannot drop hosts recorded
    since. An entry naming a known host with another key of the same type
    refuses it; another type is merged, as OpenSSH keeps one key per type.
    Names are compared in lower case, and a hashed entry under each name this
    server writes plainly, hashed with that entry's salt. A hashed entry that
    resolves to none of them is added only when this server has no hashed or
    pattern entries of its own, which could cover the host it names. A pattern entry
    (`*`, `?`) is compared under each plain name it matches, and left out when
    this server has hashed or pattern entries it could not list.
  - *Order:* the host keys are applied before the key, since their merge is
    checked again under the lock and may still refuse. The key's public half
    is written before the private key, each replaced whole: a failure up to
    and including the private key leaves the key as it was (the `.pub` is put
    back), and the private key is the last file written. A failure after that
    -- the installed key does not load -- reports the key as installed, since
    it is, and the reason. `PartialRestore` carries both what the host keys and
    what the key came to.
  - *The replaced key is kept:* `aurcache_common::fs::keep_replaced` copies it
    beside itself, `0600`, as `<file>.replaced-<tag>` -- the key's SHA256
    fingerprint in URL-safe base64, or a digest of a file that is no key it
    can read -- and the report and the activity entry say where. The helper
    is shared so the signing key's restore can keep its own the same way. A key
    is compared by its public half, not its bytes, so the same key protected
    again with a new salt is not written, nor kept as replaced.
  - *Only checkable keys:* a key in an older (PEM) format, encrypted or not,
    cannot be shown to be a key that opens before it is written -- its `.pub`
    would be taken on its word -- so a restore refuses it, suggesting
    `ssh-keygen -p` to convert it. After a key
    is installed, the server reloads it and reports "installed" only if what
    it loaded is that key.
  - *`--clear` takes what the dump carries:* the dump's key and `known_hosts`
    when it has them, the replaced `known_hosts` kept beside it as a replaced
    key is. A dump without them leaves this server's key and `known_hosts`, as
    it leaves the CA: a cleared server that regenerated its key would need it
    added to every remote again, which a restore exists to avoid.
  - *Operator paths are never written.* A key named by `AURCACHE_GIT_SSH_KEY`
    stays in use and untouched, compared by real path so that one mounted at
    the server's own key file is not written either; the dump's key then goes
    beside it, or nowhere -- nowhere too when it is the operator's key itself,
    as in a dump from the same server, so no second copy of it is written --
    and an activity entry
    (`restore.ssh_key_superseded`) says when the two differ. A replaced key
    is `restore.ssh_key_replaced`, with where the old one is kept. A pinned
    `AURCACHE_SSH_KNOWN_HOSTS` is left alone.
- **Archive modes:** the SSH key, the CA key and the token hashes are written
  `0600` in the archive (`append_private`), as the signing key is.
- **Validation:** SSH files in a dump without the CA are refused, as tokens
  are -- secrets travel together -- and so is a public key without its private
  key. The schema version is unchanged: the files are optional, and an older
  importer reading a newer dump only drops them, which leaves it where it was
  before this existed.

## Not covered

- One key for every SSH remote. Choosing a key per repository (GitHub's deploy
  keys are each bound to one) would mean named keys and rules mapping a remote
  to a key; they could join `SshInfo` as an optional field beside `key`, and
  the CLI's `ssh` group has room for `aurcli ssh keys`.
- The VCS check does not resolve SSH sources a PKGBUILD names (see Roles).
