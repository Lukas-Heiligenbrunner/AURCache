---
sidebar_position: 1
---

# CLI overview

`aurcli` is the typed CLI for the HTTP API (`cargo run -p aurcache-cli --
...`); the reusable Rust client library backing it lives in
`backend/aurcache-client`. It authenticates via `Authorization: Bearer <token>`.

## Installing

On Arch, from the AUR: `paru -S aurcache-cli`.

Elsewhere, CLI releases (tagged `aurcache-cli-v<version>`) carry a prebuilt
`aurcli` for Linux (static, so any distribution) and macOS. The
[releases page, searched for `aurcache-cli`](https://github.com/Lukas-Heiligenbrunner/AURCache/releases?q=aurcache-cli)
lists them.

| System                | Target                           |
|-----------------------|----------------------------------|
| Linux x86_64          | `x86_64-unknown-linux-musl`      |
| Linux aarch64         | `aarch64-unknown-linux-musl`     |
| Linux armv7           | `armv7-unknown-linux-musleabihf` |
| macOS (Apple Silicon) | `aarch64-apple-darwin`           |
| macOS (Intel)         | `x86_64-apple-darwin`            |

Set `version` to the release's, download its archive and checksum, check one
against the other, and install the binary:

```bash
version=X.Y.Z
target=x86_64-unknown-linux-musl
base=https://github.com/Lukas-Heiligenbrunner/AURCache/releases/download/aurcache-cli-v$version
curl -fLO "$base/aurcli-$target.tar.gz"
curl -fLO "$base/aurcli-$target.tar.gz.sha256"
sha256sum -c "aurcli-$target.tar.gz.sha256"   # macOS: shasum -a 256 -c ...
tar -xzf "aurcli-$target.tar.gz" aurcli
mkdir -p ~/.local/bin
install -m 755 aurcli ~/.local/bin/aurcli
```

The macOS binaries are not notarized by Apple, so one downloaded through a
browser is quarantined; `xattr -d com.apple.quarantine aurcli` releases it.
`curl` sets no quarantine.

`aurcli` trusts the system's certificate store, so a minimal container image
needs its CA certificates (`ca-certificates` on Debian and Ubuntu) before it
can reach a server over HTTPS.

With a Rust toolchain, build it from source instead:
`cargo install --locked --git https://github.com/Lukas-Heiligenbrunner/AURCache aurcache-cli`.

## Connecting

Every command resolves `--url` / `--token`, then `AURCACHE_URL` /
`AURCACHE_TOKEN`, then `~/.config/aurcache-client/config.json`, then an
interactive prompt (saved back to the config file):

```bash
aurcli config show                    # where the file is and what it holds
aurcli config set-url https://aur.example.com:8080/api   # the API, not the web UI
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
