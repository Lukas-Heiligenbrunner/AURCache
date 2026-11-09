# Several API tokens per user, each with a subset of the user's rights

Status: **Proposed** · Last updated: 2026-10-15

Each user has one API token, and it can do everything that user can. A script
that only reads the build list, a CI job that only queues builds, and a
review tool that should advise but never release
(`package-suspicion-signals.md`, 6) all get the same key to everything, and
replacing it for one replaces it for all. This gives each user any number of
named tokens, each limited to the scopes it was created with.

---

## Where things stand

- `api_tokens (id, username UNIQUE, token_hash UNIQUE)`
  (`aurcache-db/src/api_tokens.rs`): one row per user, the SHA-256 of 256
  random bits (`aurcache-api/src/auth.rs`, `generate_api_token`,
  `hash_api_token`).
- `POST /token/regenerate` (`aurcli token regenerate`) replaces it. Nothing
  lists, names, revokes or limits it.
- `Authenticated` (`aurcache-api/src/models/authenticated.rs`) takes a
  session cookie or `Authorization: Bearer <token>`, and yields a username.
  Every endpoint takes it, so whoever passes is allowed everything.
- **With OAuth off, `Authenticated` admits every request**, token or not.
  Tokens only mean something on an instance with OAuth on
  (`builds-and-the-api.md`, option A, is about changing that).
- The dump carries token hashes when asked to (`aurcache-utils/src/dump.rs`,
  `restore.rs`).
- `plugins.md` (3, "Identity and scopes") proposes scopes for plugins, a
  `Principal` that can be a user or a plugin, and an `Authorized<scope>`
  guard that endpoints declare. Nothing of it is built.

Tokens were added after the fork (`3a08322e`), so their routes and shapes
change freely here; the table needs a migration, since instances have rows in
it.

## The design

### Tokens

`api_tokens` becomes one row per token:

| column | |
|---|---|
| `id` | |
| `username` | the owner; no longer unique |
| `name` | the owner's label for it, unique per user: `laptop`, `ci`, `review-bot` |
| `token_hash` | SHA-256 of the secret, as today; unique |
| `scopes` | `all`, or a `;`-joined list of scopes, as `aurcache_db::lists` joins other list columns |
| `created_at` | |
| `expires_at` | optional; a token past it is refused |
| `last_used_at` | when it last authenticated a request, at most a minute stale |

**`all` is not a list of every scope.** A full token gets the scopes added in
later releases too, as its owner does. A token created with a list keeps that
list, and a new scope never widens it.

**The secret is shown once**, at creation or rotation, and never again: the
server keeps only its hash, as today. New secrets carry a recognisable prefix,
`aurcache_` then 64 hex characters, so secret scanners and people can tell
what a leaked string is. Existing tokens keep working unprefixed.

`last_used_at` is written by one task fed through a channel, not by the
request guard (the database is written by whoever owns the write, never at
call sites), and only when the stored value is a minute old or more, so a
busy script does not turn every request into a write.

### Scopes

One scope catalogue, shared with `plugins.md`: a `Scope` enum in
`aurcache-common`, used by plugin principals and by tokens alike, with the
names `plugins.md` already gives them:

| Scope | Allows |
|---|---|
| `packages:read` | listing packages, their details, settings and sources |
| `packages:write` | adding, updating, patching, removing packages and their settings |
| `builds:read` | listing builds, their details, signals, diffs, logs and artifacts |
| `builds:retry`, `builds:cancel` | queueing and stopping builds |
| `builds:release` | releasing and discarding held builds, and acknowledging warnings |
| `workers:read` | the workers and their configuration |
| `workers:pause` | pausing and resuming workers |
| `workers:manage` | approving, revoking and configuring workers |
| `settings:write` | changing server-wide settings |
| `events:read` | the activity log |
| `stats:read` | the dashboard's statistics |
| `annotations:write` | leaving and removing annotations |

`builds:release` and `workers:manage` are new: `plugins.md` did not need them,
since a plugin should never release a build or enrol a worker. A token may.

A few things have **no scope**, and are reachable only by a full token or a
session: managing tokens, exporting and restoring dumps (which can carry
the CA key and every token hash), and anything a future endpoint does before
someone decides which scope it needs.

### Checking them

`Authenticated` becomes the `Principal` of `plugins.md`, with what a user is
allowed attached:

```rust
pub enum Principal {
    /// OAuth off: anyone, allowed everything, as today.
    Open,
    /// A session cookie, or a token.
    User { username: String, token: Option<TokenRef>, scopes: Scopes },
    Plugin { id: i32, name: String, scopes: Scopes },
}
```

A session is `Scopes::All`; a token carries its own. Each endpoint declares
the scope it needs, `_a: Authorized<scope::BuildsRelease>`, which admits a
principal holding it and answers `403` naming the scope otherwise. An
endpoint that declares none admits only a principal with `Scopes::All`.
So adding an endpoint closes it to limited tokens until someone decides
which scope covers it, which is the direction to fail in.

This is the same guard `plugins.md` needs. Built once, here first, it is
what plugins then use, with their own principal.

**With OAuth off nothing changes**: every request is `Principal::Open`,
allowed everything, and a limited token is no more limited than no token at
all. Scopes limit what a token can do; they do not secure an open instance.
The token page says so on such an instance, rather than letting someone
create a "read-only" token that protects nothing.

### Routes and commands

| Route | CLI | Does |
|---|---|---|
| `GET /api/tokens` | `aurcli token list` | the caller's tokens: name, scopes, created, expires, last used. Never a secret |
| `POST /api/tokens` | `aurcli token create <name> [--scopes builds:read,annotations:write] [--expires 90d]` | creates one, and returns its secret once. Without `--scopes`, a full token |
| `POST /api/tokens/<id>/rotate` | `aurcli token rotate <name>` | replaces its secret, keeping its name, scopes and expiry, and returns the new one once |
| `DELETE /api/tokens/<id>` | `aurcli token revoke <name>` | revokes it at once |

All four need a full token or a session. A limited token cannot create a
token, so it cannot widen itself, nor mint a copy that outlives its own
revocation. A user sees and manages only their own tokens.

`POST /token/regenerate` and `aurcli token regenerate` go: rotation replaces
them, and a token is now one of several, so "the" token no longer exists.
The CLI's configuration keeps storing one token, whichever the user gives it.

The settings page lists the signed-in user's tokens, with Create, Rotate and
Revoke, and shows a new secret once, with a copy button and the warning
that it will not be shown again.

### Who did it

The activity log records each token created, rotated and revoked, by name.
Anything done with a token is recorded as done by its user, with the token
named (`alice (ci)`), since `ActivityLog::emit_by` already takes an actor,
so "which script released this" has an answer.

### Migration and dumps

- The migration drops the unique constraint on `username`, adds the new
  columns, and names each existing token `default` with `scopes = all`, so
  every existing token keeps working as it does.
- The dump and restore carry the new columns beside the hash, under the same
  "secrets" option as today.

## What this does not do

- **Expiry by default.** A token lives until revoked unless created with
  `--expires`. Expiring by default breaks scripts on a schedule nobody chose.
- **Per-package scopes** (`packages:write` on `foo` only). Possible later as
  a scope argument; nothing has asked for it.
- **Tokens for workers.** Workers authenticate with their certificates, over
  their own protocol, and stay that way.

## Open questions

1. Should a token's scopes be editable after creation, or only chosen at
   creation (and changed by creating another)? Fixed scopes are simpler to
   reason about: a token's rights are what they were when its secret was
   handed out.

## Implementation order

1. The migration, the `Scope` enum and `Scopes` list type, and
   `Principal` with `Authorized<scope>`, every endpoint declaring its scope.
   Full tokens and sessions behave exactly as today.
2. The token routes and `aurcli token`, replacing `token regenerate`, and
   `last_used_at`.
3. The settings page.

## Verification

- A limited token reaches the endpoints its scopes cover and gets `403`,
  naming the scope, on the others; an endpoint declaring no scope refuses
  it.
- A limited token cannot list, create, rotate or revoke tokens, nor export a
  dump.
- After the migration, an existing token is `default`, `all`, and reaches
  everything it did.
- A revoked or expired token is refused at once; a rotated one's old secret
  is refused.
- A test lists every mounted route and fails on one that declares no scope
  and is not on the reviewed list of full-token-only routes, so a new
  endpoint cannot be forgotten.
- With OAuth off, a request with no token still reaches everything.
