# aurcli — CLI client for the AURCache API

A typed CLI for the AURCache HTTP API. The package is `aurcache-cli`; the
binary it installs is `aurcli`:

```bash
cargo install --git https://github.com/Lukas-Heiligenbrunner/AURCache aurcache-cli
```

## Configuration

Commands that talk to the server need a URL and usually a token. Each can
come from four places, in this order:

1. Flags: `--url` / `--token`
2. Environment: `AURCACHE_URL` / `AURCACHE_TOKEN`
3. Config file: `aurcache-client/config.json` under the OS config directory
   (`~/.config` on Linux), shaped like
   ```json
   {"url": "http://localhost:8080/api", "token": "..."}
   ```
4. Interactive prompt (defaulting to `http://localhost:8080/api` for the URL),
   whose answers are saved back to the config file so it only asks once.

Without a terminal and with nothing configured, the command fails and tells
you which of the above to set. Manage the file directly with:

```bash
aurcli config show        # location, URL, and whether a token is stored (never the token itself)
aurcli config set-url http://localhost:8080/api
aurcli config set-token   # prompts, so the token never lands in shell history
```

If a request is rejected (401), the CLI prompts for a fresh token and saves
it, rather than making you re-run the command.

## Token

The token is a personal API token sent as `Authorization: Bearer <token>`.
To get your first one, open the web UI and use **Generate new token** in the
*Personal API token* section of the settings page — copy it at once, it is
shown only once, and generating replaces any existing token immediately.
Then store it with `aurcli config set-token`, export `AURCACHE_TOKEN`, or
pass `--token`. (An empty token means the server runs with auth disabled.)

Once a token works, `aurcli token regenerate` mints a replacement from the
terminal.

## Setup

The onboarding commands run before any token exists, so they need none —
they do offline arithmetic over flags and print or run containers:

- `aurcli setup compose [--role bundle|backend|worker] [--database postgres|sqlite] [-o file]`
  writes a compose file: `bundle` (the default) is server plus one local
  worker, `backend` is the server alone, `worker` joins a server elsewhere.
- `aurcli setup server` / `aurcli setup worker` run the containers on this
  machine with `docker run`. The local pair shares a network and an enroll
  volume, so the worker approves itself — no approve click.

After that, `aurcli doctor` walks server → token → workers and says what is
missing, and `aurcli worker approve <id>` admits a remote worker.

## Other commands

`pkg`, `builds` and `worker` manage packages, builds and workers;
`repo config [--install]` prints (or installs) the `pacman.conf` stanza;
`dump` / `restore` move server state; `search`, `stats`, `user-info` and
`health` read it; `completions <shell>` prints a completion script; `raw`
calls an API path with no dedicated subcommand. `--format json` makes the
typed commands machine-readable.

The full walkthrough lives in the
[quick-start](https://lukas-heiligenbrunner.github.io/AURCache/docs/overview/quick-start).
