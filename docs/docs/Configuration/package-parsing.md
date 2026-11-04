---
sidebar_position: 5
---

# Package parsing

Reading a PKGBUILD means *sourcing* it, so every package the server inspects
runs bash in a process that holds the database credentials and owns the
repository. The server never runs `alpm-pkgbuild-bridge` directly: it runs it
through `aurcache-sandbox`, naming both by absolute path, and refuses to parse
at all if that binary is missing. Nothing depends on `PATH`, so nothing about
the environment can quietly unconfine a parse. This holds for every install
method — the server image carries the same sandbox.

A parse:

- reads its PKGBUILD, and cannot read the server's state: the database, the
  worker CA, build logs, the source cache (other packages' checkouts, private
  ones included), or `/etc/aurcache`, where `server.env` keeps the database
  password and the OAuth secret;
- cannot write the server's files;
- cannot signal or inspect the processes around it;
- sees none of the server's environment;
- cannot reach the Docker socket.

How that is enforced depends on the install, and only the sandbox knows:

- **In the container images** the server runs as root, and each parse runs as
  a separate user, `aurcache-parse`, so ordinary permissions do the work on
  any kernel. The server's state directories must be closed to other users and
  not owned by `aurcache-parse`; the images set this at every start, and the
  sandbox checks it before every parse, refusing with the directory and its
  mode if one is not. Where the kernel has Landlock (check
  `/sys/kernel/security/lsm`), it is layered on top: the parse then writes
  nothing at all and cannot open a TCP connection.
- **A native install** runs the server unprivileged, so Landlock is all there
  is: it needs Linux 6.12 or newer with Landlock enabled, and the server
  refuses to parse otherwise.

Denying TCP is hardening rather than a boundary: Landlock covers TCP only, and
only from Linux 6.7, so on Synology, whose kernels predate Landlock, a parse
can connect out. It is there because a parse has no reason to connect
anywhere, and it finds nothing worth sending: no secrets in its environment,
no credentials it can read. If a package could be added that you would mind
phoning home, a host firewall is the control that stops it.

## When a package needs the network to parse

About one AUR package in five hundred computes its version while being sourced,
with `pkgver=$(curl -s https://api.github.com/…)` or `git ls-remote`. Denied the
connection, `pkgver` comes out empty and the parse fails on the missing version
rather than on the connection — so the error says which command wanted the
network and names the setting below.

`parse_network` (or `PARSE_NETWORK=true`) lifts the TCP denial for parsing.
Everything else stays: the parse still cannot read or write the server's
state, still gets no secrets in its environment, and is still isolated from the
processes around it. It is a global setting today,
so turn it on only if you would run those packages' `pkgver` command yourself.
Set it on the Settings page, or in the environment.

`AURCACHE_PROTECTED_DIR` names further directories to keep unreadable
(colon-separated). It adds to the list above rather than replacing it. In the
container images these must be closed to other users too.
