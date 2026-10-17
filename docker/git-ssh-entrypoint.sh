#!/bin/sh
# Build both repositories from scratch and serve them. Everything here is
# ephemeral: the container is recreated for each test run.
set -eu

MARKER="${MARKER:-aurcache-ssh-credential-reached-the-chroot}"

ssh-keygen -A                      # host keys (new per run; clients use accept-new)
install -d -m 700 -o git -g git /home/git/.ssh
install -m 600 -o git -g git /keys/id_ed25519.pub /home/git/.ssh/authorized_keys

export GIT_AUTHOR_NAME=aurcache GIT_AUTHOR_EMAIL=test@localhost
export GIT_COMMITTER_NAME=aurcache GIT_COMMITTER_EMAIL=test@localhost

work="$(mktemp -d)"
rm -rf /srv/git
install -d /srv/git

# The payload the build fetches over SSH. Its marker ends up inside the built
# package, so the test can prove the fetch actually happened.
install -d "$work/payload"
printf '%s\n' "$MARKER" > "$work/payload/marker.txt"
git init -q -b master "$work/payload"
git -C "$work/payload" add -A
git -C "$work/payload" commit -qm "payload"
git clone -q --bare "$work/payload" /srv/git/payload.git

# The packaging repo AURCache clones, unauthenticated.
install -d "$work/pkg"
cp /fixture/PKGBUILD "$work/pkg/PKGBUILD"
git init -q -b master "$work/pkg"
git -C "$work/pkg" add -A
git -C "$work/pkg" commit -qm "packaging"
git clone -q --bare "$work/pkg" /srv/git/pkg.git

chown -R git:git /srv/git

/usr/sbin/sshd -e
# Drop to the repo owner: git refuses to serve a repository owned by another
# user ("dubious ownership"), and the daemon would otherwise run as root.
exec git daemon --base-path=/srv/git --export-all --reuseaddr --listen=0.0.0.0 \
    --user=git --group=git
