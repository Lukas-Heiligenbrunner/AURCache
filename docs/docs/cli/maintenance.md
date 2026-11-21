---
sidebar_position: 5
---

# Backup and restore

```bash
aurcli dump                              # authored state to a dated .tar.gz
aurcli dump --include-secrets -o backup.tar.gz
aurcli restore backup.tar.gz --dry-run
aurcli restore backup.tar.gz --on-existing skip
aurcli restore backup.tar.gz --clear     # replace: removes every package, setting and worker first,
                                         # and leaves the signing key as a fresh install has it
```

`--include-secrets` also exports the CA private key, worker certificates, API
token hashes and the repository's [signing
key](../Configuration/pacman-repo.md#the-key) — the file becomes a credential
and is written owner-readable only, its secret files mode 0600 inside the
archive too. Taking the dump's secrets on restore
(`--secrets copy`) replaces the CA, which invalidates every certificate the
server's workers hold, and puts the signing key in place, so a reinstalled or
moved instance signs with the key its clients already trust.

:::warning Keep a dump with secrets as safely as the signing key
Whoever holds it can sign packages that every client trusting the repository
installs as root, until the key is replaced and every client trusts the new
one. A key locked with a passphrase
(`AURCACHE_SIGNING_KEY_PASSPHRASE_FILE`) stays locked in the dump, and the
passphrase is never in it: kept apart from the passphrase, the dump alone signs
nothing. Without a passphrase the key is in the dump as it is on disk, and the
export says so in the server's log.
:::

:::danger Without sign-in, anyone who can reach the API can take a dump with secrets
An instance with no [sign-in](../Configuration/authentication.md) configured
treats every caller as signed in. Anyone who can reach its API can then call
`GET /api/dump?include_secrets=true` and get the worker CA's private key and
the signing key — unlocked, unless it is locked with a passphrase — and can
call `POST /api/restore?secrets=copy` to put a CA and a signing key of their
own in place. Either way they can sign packages your clients install as root,
and mint workers your server trusts. Before signing a repository, or keeping
any secret on such an instance worth protecting, configure sign-in, or keep
the API unreachable from anyone you would not hand the signing key to: bound
to localhost or a private network, or behind a proxy that authenticates.
:::

The signing key in a dump is the one the server signs with, its own or one
supplied with `AURCACHE_SIGNING_KEY_FILE`, exactly as stored. A dump without
secrets says nothing about signing at all, and restores anywhere. On restore:

- **Checked first.** A restore whose secrets cannot all be put in place is
  refused before anything changes — a CA that is not a pair, a CA directory
  it cannot write, a key file mounted where the server keeps its own (name
  such a key with `AURCACHE_SIGNING_KEY_FILE` instead) — and so is a key it
  cannot use as it would be used. The passphrase file is read only when a key
  is written, so a restore that writes none goes through whatever has become
  of it:
  - one the server is to sign with has to unlock with the passphrase this
    server has configured, and to sign now;
  - one only kept, for a server that signs with a supplied key or not at
    all, has to unlock with this server's passphrase if it has one, and need
    not sign now;
  - one not written at all — where the server's own file is the one it is
    supplied — is not checked.

  A key that comes unlocked onto a server with a passphrase configured is
  written locked with it. A dump whose key has expired since still restores,
  except as the key to sign with. `--dry-run` says the same, naming what is
  missing, and lists nothing else when it refuses. Everything new is written
  beside what it replaces before the packages are imported, and renamed into
  place after; should a rename still fail, the restore says which secret was
  not put in place, and that one is as it was. A rename that succeeded whose
  directory could not be synced after is in place, and the restore says a
  crash soon after may undo it. One restore runs at a time: another is
  refused until it ends.
- **Reported.** `--dry-run` and the restore both say, beside the packages,
  what becomes of the worker CA and of the signing key — `kept`, `restored`,
  `replaced`, `generated` or `removed` — with the fingerprints, and where the
  key that is replaced or removed goes.
- **Only once the packages are known to be in.** If the database cannot
  confirm that the packages committed — a connection lost while committing,
  and the server unable to read back what it wrote — the restore says the
  outcome is unknown, and leaves the CA and the signing key as they are.
  Check which of the dump's packages are there, then restore it again, with
  `--on-existing overwrite` if any are.
- **`--secrets copy`** puts the dump's key in the server's own key location,
  replacing a key a fresh instance generated, and the server signs with it at
  once: what the replaced key signed is signed again in the background.
- **`--clear`** leaves the key as a fresh install has it: the dump's when it
  carries one; otherwise a server that signs with its own key gets a new one
  in its place at once, which every client then has to trust, and one that
  does not sign has its own key file removed.
- **Neither** leaves the key as it is.
- A key supplied with `AURCACHE_SIGNING_KEY_FILE` is the operator's: a restore
  never writes or removes it, even where the server keeps its own — linked or
  mounted there, it is the same file — and it keeps signing; the log says when
  it is not the dump's.

A key file a restore replaces or removes is kept beside it, owner-only, named
for the key it held: `signing-key.asc.replaced-<fingerprint>` in the CA
directory (`.2`, `.3` and so on after it when that name holds another file
already). The restore's report says where it went. Clients still have to
trust the key the server signs with after; the kept one is there to go back
to, or to sign with elsewhere. A file that does not read as a key is kept as
`signing-key.asc.replaced-sha256-<16 hex digits of its SHA-256>`. The worker CA a restore replaces is not
kept.

Values set on workers from AURCache are included in a backup with the worker
they belong to, and restored with it.

`aurcli raw` calls an arbitrary API path for endpoints without a dedicated
subcommand:

```bash
aurcli raw GET /packages/list
aurcli raw POST /workers/3/approve
```
