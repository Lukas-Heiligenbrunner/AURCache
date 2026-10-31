#!/bin/bash
# Install AURCache's packaging files into $pkgdir.
#
# Shared by the server and worker PKGBUILDs. They differ in which binaries and
# units they carry; everything else about what a package *is* -- the users, the
# directories, the modes -- lives here, so the two cannot drift on ownership of
# the paths they share. The container images install these same packages, which
# is what stops an image being a second, hand-maintained copy of the host
# contract.
#
# Usage: install-files.sh sandbox|server|worker|worker-docker|cli <pkgdir> <repo-root> <binary-dir>
set -euo pipefail

readonly ROLE=$1
readonly PKGDIR=$2
readonly ROOT=$3
readonly BINDIR=$4

readonly PKG="$ROOT/packaging"

# The package being built, which is also the name its declaration files take.
case "$ROLE" in
sandbox)       _pkgname=aurcache-sandbox ;;
server)        _pkgname=aurcache-server ;;
worker)        _pkgname=aurcache-worker ;;
worker-docker) _pkgname=aurcache-worker-docker ;;
cli)           _pkgname=aurcache-cli ;;
*)  echo "install-files.sh: unknown role '$ROLE'" >&2; exit 1 ;;
esac

# Each package installs its declarations under its OWN name, even where two
# packages share the source file. pacman refuses to install two packages that
# ship the same path -- identical contents are no exemption, it is a plain path
# conflict -- so a single `aurcache.conf` would make the packages mutually
# exclusive. systemd merges every file in these directories and both tools are
# idempotent, so declaring one user or directory twice is a no-op.
#
# `$_decl` names the source file; the installed name is always the package.
# aurcache-sandbox and aurcache-cli are exempt: each is one binary the other
# packages do not depend on, owning no users, directories or state of its own.
if [ "$ROLE" != sandbox ] && [ "$ROLE" != cli ]; then
    case "$ROLE" in
    server) _decl=aurcache-server ;;
    *)      _decl=aurcache-worker ;;   # both workers share the worker declarations
    esac
    install -Dm644 "$PKG/$_decl.sysusers" "$PKGDIR/usr/lib/sysusers.d/$_pkgname.conf"
    install -Dm644 "$PKG/$_decl.tmpfiles" "$PKGDIR/usr/lib/tmpfiles.d/$_pkgname.conf"
fi

case "$ROLE" in
sandbox)
    install -Dm755 "$BINDIR/aurcache-sandbox" "$PKGDIR/usr/bin/aurcache-sandbox"
    ;;
server)
    install -Dm755 "$BINDIR/aurcache" "$PKGDIR/usr/bin/aurcache"
    install -Dm644 "$PKG/aurcache.service" \
        "$PKGDIR/usr/lib/systemd/system/aurcache.service"
    install -Dm644 "$PKG/server.env" "$PKGDIR/etc/aurcache/server.env"
    ;;
worker)
    install -Dm755 "$BINDIR/aurcache-worker" "$PKGDIR/usr/bin/aurcache-worker"
    install -Dm644 "$PKG/aurcache-worker.service" \
        "$PKGDIR/usr/lib/systemd/system/aurcache-worker.service"

    # The patcher and its wrapper, plus the hook that re-runs them when devtools
    # changes. The derived copy is deliberately not shipped: it has to come from
    # the devtools installed on the machine, or it would run against another
    # release's libraries.
    install -Dm644 "$PKG/patch-makechrootpkg.py" \
        "$PKGDIR/usr/lib/aurcache/patch-makechrootpkg.py"
    install -Dm755 "$PKG/aurcache-patch-makechrootpkg" \
        "$PKGDIR/usr/lib/aurcache/aurcache-patch-makechrootpkg"
    install -Dm644 "$PKG/aurcache-makechrootpkg.hook" \
        "$PKGDIR/usr/share/libalpm/hooks/aurcache-makechrootpkg.hook"

    # Paths a PKGBUILD must never read; see the file for why it is not an
    # environment variable.
    install -Dm644 "$PKG/sandbox-protected" "$PKGDIR/etc/aurcache/sandbox-protected"
    install -Dm644 "$PKG/worker.env" "$PKGDIR/etc/aurcache/worker.env"
    install -Dm440 "$PKG/aurcache-worker.sudoers" \
        "$PKGDIR/etc/sudoers.d/aurcache-worker"
    ;;
worker-docker)
    # The legacy executor delegates builds to containers, so it needs none of
    # the chroot machinery the chroot worker installs -- no sandbox, no patched
    # makechrootpkg, no sudoers entry. Just the binary and the shared users and
    # directories declared above.
    install -Dm755 "$BINDIR/aurcache-worker-docker" \
        "$PKGDIR/usr/bin/aurcache-worker-docker"
    ;;
cli)
    # The client is one binary with no service, users or state of its own.
    install -Dm755 "$BINDIR/aurcli" "$PKGDIR/usr/bin/aurcli"
    ;;
*)
    echo "install-files.sh: unknown role '$ROLE'" >&2
    exit 1
    ;;
esac
