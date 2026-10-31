---
sidebar_position: 1
---

# CLI overview

`aurcli` is the typed CLI for the HTTP API (`cargo run -p aurcache-cli --
...`); the reusable Rust client library backing it lives in
`backend/aurcache-client`. It authenticates via `Authorization: Bearer <token>`.

## Connecting

Every command resolves `--url` / `--token`, then `AURCACHE_URL` /
`AURCACHE_TOKEN`, then `~/.config/aurcache-client/config.json`, then an
interactive prompt (saved back to the config file):

```bash
aurcli config show                    # where the file is and what it holds
aurcli config set-url https://aur.example.com:8080
aurcli config set-token <token>
aurcli user-info                       # who this token authenticates as
aurcli token regenerate                # rotate the current user's token
```

Use `--format json` for machine-readable output, or `raw` for endpoints without
a dedicated subcommand:

```bash
aurcli raw GET /packages/list
aurcli completions bash                # also zsh, fish, elvish, powershell
```

Onboarding commands run before the client is built, so they need no token:
`setup`, `repo config`, and `completions`.

## Sections

- [Setup and health](./setup.md) — `setup`, `health`, `doctor`
- [Packages and builds](./packages.md) — `pkg`, `builds`, `search`, `stats`, `graph`
- [Workers](./workers.md) — `worker`
- [Backup and restore](./maintenance.md) — `dump`, `restore`, `raw`
