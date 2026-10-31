---
sidebar_position: 4
---

# Pacman Repository

Add the following to your `/etc/pacman.conf` on your target machine to use the repo:

```bash
# nano /etc/pacman.conf
[repo]
SigLevel = Optional TrustAll
Server = http://<server_ip>:8081/$arch
```

## Let the CLI fill it in

`aurcli` is already configured to talk to the instance, and the repository
is the same host on the mirror port — so it can print the stanza with nothing
left to substitute:

```bash
$ aurcli repo config
[repo]
SigLevel = Optional TrustAll
Server = http://192.168.1.10:8081/$arch
```

Or have it make the change:

```bash
aurcli repo config --install
sudo pacman -Sy
```

`--install` appends the stanza to `/etc/pacman.conf` and is safe to run twice —
if the repository is already configured it says so and changes nothing. It tries
the write unprivileged first, so running as root, or against a file you own, never
prompts; only a permission error falls back to `sudo`, which asks for your
password on the terminal. `--pacman-conf` points it at a different file, for a
container or a chroot.

Without `--install` the stanza goes to stdout and the instructions to stderr, so
it also appends cleanly by hand:

```bash
aurcli repo config | sudo tee -a /etc/pacman.conf
```

With a token configured, it asks the server how it actually publishes the
repository rather than assuming the default port — a deployment behind a reverse
proxy, on another port, or under a path is described correctly. The host is still
decided locally, because the server only sees how *it* was reached: a published
address that names a real host is kept, and the `localhost` default is replaced
with the address that works from here.

Without a token it falls back to the configured URL's host on the default port,
so it still works before the instance is otherwise set up. `--offline` forces
that fallback; `--port`, `--name` and `--siglevel` override the result outright.

`$arch` is pacman's own variable and is deliberately left unexpanded: one stanza
serves every architecture the instance builds for.
