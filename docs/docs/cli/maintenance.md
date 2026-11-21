---
sidebar_position: 5
---

# Backup and restore

```bash
aurcli dump                              # authored state to a dated .tar.gz
aurcli dump --include-secrets -o backup.tar.gz
aurcli restore backup.tar.gz --dry-run
aurcli restore backup.tar.gz --on-existing skip
aurcli restore backup.tar.gz --clear     # replace: removes every package, setting and worker first
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
An instance with no [sign-in](../Configuration/authentication.md#without-sign-in)
treats every caller as signed in, so anyone who can reach its API can take the
CA and the signing key, or restore their own. Configure sign-in, or keep the
API unreachable, before keeping a secret there worth protecting.
:::

The signing key in a dump is the one the server signs with, its own or one
supplied with `AURCACHE_SIGNING_KEY_FILE`, exactly as stored. On restore:

- **Checked first.** A restore whose secrets cannot all be put in place is
  refused whole, `400` `nothing was imported: …`, `--dry-run` included: a CA
  that is not a pair, a CA directory it cannot write, a key file mounted
  where the server keeps its own (name it with `AURCACHE_SIGNING_KEY_FILE`
  instead), or a key it cannot use: one to sign with has to unlock with this
  server's passphrase and sign now, one only kept has to unlock with it if
  there is one. A key that comes unlocked onto a server with a passphrase is
  written locked with it. One restore runs at a time.
- **Reported.** The answer says, apart from the packages, what becomes of
  each secret — `kept`, `restored` or `replaced` — and where a replaced key
  goes. A rename that still fails after the packages are in is a failed
  entry, and that secret is as it was; when the commit's outcome is unknown,
  no secret changes.
- **`--secrets copy`**, and **`--clear`** with a dump that carries a key, put
  the dump's key in the server's own key file, and the server signs with it
  at once; what the old key signed is signed again in the background.
  Without a key in the dump, or without either option, the key is left as
  it is.
- A key supplied with `AURCACHE_SIGNING_KEY_FILE` is never written, even
  where the server keeps its own — linked or mounted there, it is the same
  file — and keeps signing.

The key file a restore replaces is kept beside it, owner-only, as
`signing-key.asc.replaced-<fingerprint>` (`.2`, `.3` after it when that name
is taken, `replaced-sha256-<16 hex digits>` for a file that is not a key),
unless it held the very bytes written. The worker CA a restore replaces is
not kept.

Values set on workers from AURCache are included in a backup with the worker
they belong to, and restored with it.

`aurcli raw` calls an arbitrary API path for endpoints without a dedicated
subcommand:

```bash
aurcli raw GET /packages/list
aurcli raw POST /workers/3/approve
```
