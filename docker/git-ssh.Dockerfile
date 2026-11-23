# syntax=docker/dockerfile:1
#
# Throwaway git server for the SSH-credential end-to-end test.
#
# Serves three repositories:
#
# * `pkg.git`   over `git://` (unauthenticated) — the packaging repo that
#   AURCache itself clones. The server needs no SSH credential for it.
# * `payload.git` over `ssh://` — what the PKGBUILD fetches *during the build*,
#   so only the worker needs the key. That split mirrors the real case
#   (`unreal-engine`), where the AUR holds the PKGBUILD publicly and the sources
#   sit behind an authenticated remote.
# * `/srv/ssh-only/pkg.git` over `ssh://` and nothing else — a private
#   packaging repo, which the server has to clone with its own key.
#
# The authorised key is supplied at run time from a mounted directory, so the
# test can generate a fresh throwaway keypair per run and nothing is baked into
# the image or committed to the repository. The server's key is authorised
# later, by the test, once it has read it from the server.
FROM alpine:3.24

RUN apk add --no-cache openssh-server git git-daemon \
    && adduser -D -s /bin/sh git \
    # sshd refuses an account whose password field is `!` (locked), even for
    # public-key auth; `*` means "no password login" without locking it.
    && sed -i 's/^git:!/git:*/' /etc/shadow

COPY docker/testpkg/ssh-fixture/PKGBUILD /fixture/PKGBUILD
COPY docker/testpkg/ssh-source-fixture/PKGBUILD /fixture/ssh-source/PKGBUILD
COPY --chmod=0755 docker/git-ssh-entrypoint.sh /usr/local/bin/entrypoint

EXPOSE 22 9418
ENTRYPOINT ["/usr/local/bin/entrypoint"]
