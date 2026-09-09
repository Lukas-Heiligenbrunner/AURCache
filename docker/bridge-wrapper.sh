#!/bin/bash
# Confine `alpm-pkgbuild-bridge`, installed under that name so the parser
# resolves here instead of the real script.
#
# The bridge parses a PKGBUILD by *sourcing* it — `source_safe()` is a plain
# bash `source`, so every top-level statement runs, and so does any command
# substitution in an assignment inside a function body. On the server that is
# attacker-supplied code in the process that owns the package database and the
# repository.
#
# Three defences, because Landlock alone closes only one of them:
#
#   1. cwd is moved to the PKGBUILD's own directory (a short-lived temp dir
#      created per parse), so a relative-path write lands somewhere harmless.
#   2. /app is made unreadable: the package database and every built package
#      live there, so a PKGBUILD can neither read them nor rewrite one. Reads
#      are otherwise unrestricted, because enumerating what bash needs is
#      open-ended while the set to protect is one directory. Writes are denied
#      everywhere except the parse's own temp dir.
#   3. The environment is replaced. Landlock cannot protect a secret that is
#      already in memory, and the server's environment carries DB_PWD and
#      SECRET_KEY — a PKGBUILD reads those with `$DB_PWD`, touching no files.
#
# Network is deliberately unrestricted, so exfiltration is only prevented to the
# extent the secrets cannot be read in the first place.
set -euo pipefail

# Defaults are the image's layout. Both are overridable so the test suite can
# run the same wrapper against a build tree - see scripts/test-sandbox.sh.
real=${AURCACHE_PKGBUILD_BRIDGE:-/usr/local/libexec/alpm-pkgbuild-bridge}
sandbox=${AURCACHE_SANDBOX:-/usr/local/bin/aurcache-sandbox}
pkgbuild=${1:?usage: alpm-pkgbuild-bridge <path/to/PKGBUILD>}
dir=$(cd "$(dirname "$pkgbuild")" && pwd)
file=$(basename "$pkgbuild")

cd "$dir"
exec env -i \
    PATH=/usr/local/bin:/usr/bin:/bin \
    HOME=/nonexistent \
    LANG=C.UTF-8 \
    "$sandbox" \
        --read-except "${AURCACHE_PROTECTED_DIR:-/app}" \
        --allow "$dir" \
        -- "$real" "$file"
