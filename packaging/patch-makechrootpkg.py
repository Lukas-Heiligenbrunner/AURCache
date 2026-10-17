#!/usr/bin/env python3
"""Patch `makechrootpkg` for the worker: confinement, and one nspawn default.

Mostly the first. The `arch-nspawn` wrapper patch is not about confinement
and says so; it opts out of a systemd-nspawn default that is about to change.
The three after it close the other direction: links the build leaves in its
output directories, which `makechrootpkg` would otherwise `chown` through as
root on the host.

A PKGBUILD is bash. Sourcing one runs it. `makechrootpkg` does that twice
*outside* the chroot, as the build user:

  1. `download_sources()` runs `makepkg --verifysource`, and makepkg sources the
     PKGBUILD to learn what to fetch;
  2. it runs `source PKGBUILD` directly to read `pkgbase`/`pkgname`.

Unconfined, either one lets a build write every other build's files — another
package's cached sources, the shared GnuPG keyring, a concurrent job's PKGBUILD.
Checksums are no defence: they are declared by the PKGBUILD, so rewriting a
neighbour's PKGBUILD rewrites its checksums too.

Both sites are wrapped in `aurcache-sandbox`, which applies a Landlock policy
allowing writes only to that build's own directories. The chroot build itself is
untouched: Landlock refuses `mount(2)` outright, so nothing confined this way
could run `arch-nspawn`.

Writes a shadowed copy to /usr/local/bin, which precedes /usr/bin in both PATH
and sudo's `secure_path`; the packaged script is left alone.

Every patch MUST apply exactly once. If devtools moves a call site this fails
the image build rather than silently shipping a worker that executes PKGBUILDs
unconfined — the reason a patch is preferred here over shadowing `makepkg` on
PATH, whose failure mode would be invisible.
"""

import os
import stat
import sys

SRC = os.environ.get("MAKECHROOTPKG_SRC", "/usr/bin/makechrootpkg")
# A location AURCache owns, named by the worker as an absolute path.
#
# Not `/usr/local/bin`: that belongs to the administrator rather than to a
# package, and a copy there shadows `makechrootpkg` for *every* user on the
# host, so someone building something unrelated by hand silently gets this
# sandboxed variant. It was chosen originally because it is on sudo's
# `secure_path` -- the worker runs devtools through sudo, which replaces PATH --
# and naming the file absolutely removes that constraint entirely.
DST = os.environ.get("MAKECHROOTPKG_DST", "/usr/lib/aurcache/bin/makechrootpkg")

# (description, anchor, replacement)
PATCHES = [
    (
        "download_sources: makepkg --verifysource",
        '\t\tmakepkg --config="$copydir/etc/makepkg.conf" --verifysource -o',
        '\t\taurcache-sandbox --allow-build-env -- makepkg --config="$copydir/etc/makepkg.conf" --verifysource -o',
    ),
    (
        "download_sources: keep temp files inside the job's own directory",
        # makepkg's signature verification does `statusfile=$(mktemp)`, which
        # lands in /tmp — outside the sandbox's allow-list, so every package
        # with signed sources fails with "mktemp: Permission denied" followed
        # by a confusing "No such file or directory" from gpg's status parser.
        # Pointing TMPDIR at $WORKDIR keeps those files in a directory the job
        # already owns, so /tmp stays unwritable.
        '\t\tenv SRCDEST="$SRCDEST" BUILDDIR="$WORKDIR" \\',
        '\t\tenv SRCDEST="$SRCDEST" BUILDDIR="$WORKDIR" TMPDIR="$WORKDIR" \\',
    ),
    (
        # A fetch that died mid-way (a worker killed mid-download, a full
        # disk, ...) leaves a partial, unusable VCS checkout behind -- say a
        # .bzr tree staged past cloning but before finishing. makepkg reuses
        # any existing directory, so the next run tries to *update* the wreck
        # instead of fetching afresh and fails forever ("Not a branch" and the
        # like). Wipe the per-package source cache and retry exactly once from
        # a clean slate, and only when the cache already held something: a
        # deterministic failure (bad URL, checksum mismatch) should not
        # discard sources that were fine. Both attempts run the same
        # sandboxed makepkg, so the retry is Landlocked identically.
        #
        # `$SRCDEST` falls back to `$PWD` for unprivileged use, where the
        # PKGBUILD itself lives; that is never wiped.
        "download_sources: clear a poisoned source cache and retry once",
        "download_sources() {\n"
        "\tsetup_workdir\n"
        "\tchown \"$makepkg_user:\" \"$WORKDIR\"\n"
        "\n"
        "\t# Ensure sources are downloaded\n"
        "\tsudo -u \"$makepkg_user\" --preserve-env=GNUPGHOME,SSH_AUTH_SOCK \\\n"
        "\t\tenv SRCDEST=\"$SRCDEST\" BUILDDIR=\"$WORKDIR\" TMPDIR=\"$WORKDIR\" \\\n"
        "\t\taurcache-sandbox --allow-build-env -- makepkg --config=\"$copydir/etc/makepkg.conf\" --verifysource -o \"${verifysource_args[@]}\" ||\n"
        "\t\tdie \"Could not download sources.\"\n"
        "}",
        "download_sources() {\n"
        "\tsetup_workdir\n"
        "\tchown \"$makepkg_user:\" \"$WORKDIR\"\n"
        "\n"
        "\t# Ensure sources are downloaded\n"
        "\tif sudo -u \"$makepkg_user\" --preserve-env=GNUPGHOME,SSH_AUTH_SOCK \\\n"
        "\t\tenv SRCDEST=\"$SRCDEST\" BUILDDIR=\"$WORKDIR\" TMPDIR=\"$WORKDIR\" \\\n"
        "\t\taurcache-sandbox --allow-build-env -- makepkg --config=\"$copydir/etc/makepkg.conf\" --verifysource -o \"${verifysource_args[@]}\"; then\n"
        "\t\treturn\n"
        "\tfi\n"
        "\n"
        "\t# VCS fetches that die mid-way leave a broken checkout behind. makepkg\n"
        "\t# sees that directory next time and updates it instead of fetching\n"
        "\t# afresh, which is how a repository that is really a repository fails\n"
        "\t# forever as \"Not a branch\". Give a failed download exactly one\n"
        "\t# clean-slate retry -- but only when the cache already held something,\n"
        "\t# so a deterministic failure (bad URL, wrong checksum) does not throw\n"
        "\t# away sources that were fine. The working directory is never wiped:\n"
        "\t# `SRCDEST` falls back to `$PWD` for unprivileged use, where the\n"
        "\t# PKGBUILD itself lives.\n"
        "\tif [[ -d $SRCDEST && $SRCDEST != \"$PWD\" && -n $(ls -A \"$SRCDEST\" 2>/dev/null) ]]; then\n"
        "\t\twarning \"Clearing incomplete source cache and re-downloading\"\n"
        "\t\t# `rm -rf \"$SRCDEST\"/*` alone would miss dotfiles and `.*` would\n"
        "\t\t# reach `.`/`..`; find with -mindepth 1 covers both kinds and never\n"
        "\t\t# the directory itself (prepare_chroot bind-mounts it, so it must\n"
        "\t\t# survive), without following symlinks out of the cache.\n"
        "\t\tfind \"$SRCDEST\" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} + || die \"Could not clear %s\" \"$SRCDEST\"\n"
        "\t\tif sudo -u \"$makepkg_user\" --preserve-env=GNUPGHOME,SSH_AUTH_SOCK \\\n"
        "\t\t\tenv SRCDEST=\"$SRCDEST\" BUILDDIR=\"$WORKDIR\" TMPDIR=\"$WORKDIR\" \\\n"
        "\t\t\taurcache-sandbox --allow-build-env -- makepkg --config=\"$copydir/etc/makepkg.conf\" --verifysource -o \"${verifysource_args[@]}\"; then\n"
        "\t\t\treturn\n"
        "\t\tfi\n"
        "\tfi\n"
        "\n"
        "\tdie \"Could not download sources.\"\n"
        "}",
    ),
    (
        # The worker used to install this into the *base* chroot before each
        # build and let `sync_chroot` carry it into the copy. Two builds
        # starting at once then raced: the second overwrote the drop-in while
        # the first was still being copied, and a build ran with another
        # package's MAKEFLAGS, PACKAGER and ssh-agent socket. Installing into
        # `$copydir` instead makes it per-build by construction -- there is no
        # shared file left to overwrite, and no lock to hold across a copy the
        # worker cannot observe.
        #
        # It must land in the copy rather than be bind-mounted in, because
        # `download_sources` runs `makepkg --config="$copydir/etc/makepkg.conf"`
        # on the *host*, and makepkg reads `"$MAKEPKG_CONF.d"/*.conf` relative
        # to that path -- so a drop-in visible only inside the container would
        # be missed by the step that fetches sources, losing `GIT_SSH_COMMAND`
        # for authenticated fetches.
        #
        # Before `download_sources`, which is the first thing to read it, and
        # after the copy exists. Unset `AURCACHE_DROPIN` (anyone running this
        # copy by hand) leaves the chroot's own configuration untouched.
        "per-build makepkg drop-in installed into the chroot copy",
        "\ndownload_sources\n\nprepare_chroot\n",
        """
if [[ -n ${AURCACHE_DROPIN:-} ]]; then
\tinstall -Dm644 "$AURCACHE_DROPIN" "$copydir/etc/makepkg.conf.d/aurcache.conf" ||
\t\tdie "Unable to install %s" "$AURCACHE_DROPIN"
fi

download_sources

prepare_chroot
""",
    ),
    (
        # `check_root` re-execs through `sudo --preserve-env=<list>`, which
        # drops everything else. A no-op for the worker, which is already root
        # by then, but without this the drop-in -- and the cgroup placement
        # below -- silently vanish for anyone invoking the script unprivileged.
        "AURCACHE_* kept across the root re-exec",
        "check_root SOURCE_DATE_EPOCH,BUILDTOOL,BUILDTOOLVER,GNUPGHOME,SRCDEST,"
        "SRCPKGDEST,PKGDEST,LOGDEST,NPROC,MAKEFLAGS,PACKAGER",
        "check_root SOURCE_DATE_EPOCH,BUILDTOOL,BUILDTOOLVER,GNUPGHOME,SRCDEST,"
        "SRCPKGDEST,PKGDEST,LOGDEST,NPROC,MAKEFLAGS,PACKAGER,AURCACHE_DROPIN,"
        "AURCACHE_NSPAWN_KEEP_UNIT",
    ),
    (
        "direct `source PKGBUILD` for pkgbase/pkgname",
        '} < <(sudo -u "$makepkg_user" bash -c \'',
        '} < <(sudo -u "$makepkg_user" aurcache-sandbox --allow-build-env -- bash -c \'',
    ),
    (
        # Two nspawn options, neither a confinement patch, added by one wrapper.
        #
        # `--restrict-address-families=`: systemd-nspawn currently permits
        # every socket address family and warns, in every build log, that a
        # future version will default to AF_INET, AF_INET6 and AF_UNIX only.
        # Builds would lose AF_NETLINK -- which glibc uses for `getaddrinfo`'s
        # AI_ADDRCONFIG, so name resolution is implicated -- along with AF_ALG
        # and AF_PACKET. Whatever an arbitrary PKGBUILD needs, it is not for
        # this to guess, so opt out explicitly: an empty argument is the
        # documented way to keep today's behaviour, and it silences the notice
        # as a side effect. Guarded on the version because the option only
        # exists from systemd 261; a worker mid-upgrade should build rather
        # than fail on an unknown option.
        #
        # `--keep-unit`, when the worker asks for it: arch-nspawn passes
        # `--slice=devtools-$SUDO_USER`, so by default nspawn runs the container
        # in a transient scope of its own under devtools.slice -- *outside* the
        # cgroup the worker created for the build. The worker kills a build
        # with that cgroup's `cgroup.kill` and reads its `memory.peak`, so the
        # container escaped both: Stop killed makechrootpkg and left makepkg
        # and every compiler running, and the memory figure covered only the
        # host-side download. `--keep-unit` (with the `--register=no` arch-nspawn
        # already passes) keeps the container in the cgroup nspawn was started
        # from, as `payload`/`supervisor` children of the build's cgroup; it
        # also disables `--slice=`, which is the point. The container images
        # have always forced it (docker/nspawn-wrapper.sh), for want of a
        # systemd manager to allocate a scope.
        #
        # Opt-in through `AURCACHE_NSPAWN_KEEP_UNIT` rather than always on: the
        # worker sets it only when it has created a cgroup for the build, and
        # nspawn refuses `--keep-unit` from a user session, which is where
        # someone running this copy by hand would be.
        #
        # A shell function rather than five edits: makechrootpkg calls
        # `arch-nspawn` unqualified in five places, and a function of that name
        # takes precedence over the PATH lookup in all of them. The flags have
        # to land straight after the working directory, since everything after
        # it is the command to run inside the container.
        "arch-nspawn wrapper: address families, and the build's own cgroup",
        "bindmounts_ro=()",
        """_aurcache_nspawn_args=()
_aurcache_nspawn_version=$(systemd-nspawn --version 2>/dev/null \\
\t| awk 'NR==1 {print $2 + 0; exit}')
if [[ -n ${_aurcache_nspawn_version:-} ]] && (( _aurcache_nspawn_version >= 261 )); then
\t_aurcache_nspawn_args+=(--restrict-address-families=)
fi
if [[ ${AURCACHE_NSPAWN_KEEP_UNIT:-} == 1 ]]; then
\t_aurcache_nspawn_args+=(--keep-unit)
fi
if (( ${#_aurcache_nspawn_args[@]} )); then
\tarch-nspawn() {
\t\tlocal _aurcache_dir=$1
\t\tshift
\t\tcommand arch-nspawn "$_aurcache_dir" "${_aurcache_nspawn_args[@]}" "$@"
\t}
fi

bindmounts_ro=()""",
    ),
    # The three loops that hand the build's outputs back run `chown` as host
    # root on every entry of the copy's `pkgdest`, `srcpkgdest` and `logdest`.
    # The build writes those directories, and `chown` follows symlinks: a
    # PKGBUILD that leaves `/pkgdest/x -> /etc/shadow` has host root give
    # /etc/shadow to the invoking user (`aurcache`), and clears the setuid bit
    # of anything it points at. `move_logfiles` runs on a failed build too.
    # Tested against devtools 1.5.1; see design/proposed/build-runner.md.
    #
    # Skip anything that is not a regular file, and `chown -h` what is left so
    # no link is ever followed. The container has exited by now, and its
    # processes with it, so nothing can swap a file for a link in between.
    # A skipped entry is not a product makepkg made; the worker refuses
    # symlinked artifacts anyway (aurcache-worker-core/src/artifacts.rs).
    (
        "move_logfiles: never follow a link the build left in logdest",
        '\t\tchown "$src_owner" "$l"\n',
        '\t\tif [[ -L $l || ! -f $l ]]; then\n'
        "\t\t\twarning 'Skipping %s: not a regular file' \"${l##*/}\"\n"
        "\t\t\tcontinue\n"
        "\t\tfi\n"
        '\t\tchown -h "$src_owner" "$l"\n',
    ),
    (
        "move_products: never follow a link the build left in pkgdest",
        '\t\tchown "$src_owner" "$pkgfile"\n',
        '\t\tif [[ -L $pkgfile || ! -f $pkgfile ]]; then\n'
        "\t\t\twarning 'Skipping %s: not a regular file' \"${pkgfile##*/}\"\n"
        "\t\t\tcontinue\n"
        "\t\tfi\n"
        '\t\tchown -h "$src_owner" "$pkgfile"\n',
    ),
    (
        "move_products: never follow a link the build left in srcpkgdest",
        '\t\tchown "$src_owner" "$s"\n',
        '\t\tif [[ -L $s || ! -f $s ]]; then\n'
        "\t\t\twarning 'Skipping %s: not a regular file' \"${s##*/}\"\n"
        "\t\t\tcontinue\n"
        "\t\tfi\n"
        '\t\tchown -h "$src_owner" "$s"\n',
    ),
]


def main() -> int:
    text = open(SRC, encoding="utf-8").read()

    for description, anchor, replacement in PATCHES:
        found = text.count(anchor)
        if found != 1:
            print(
                f"patch-makechrootpkg: expected exactly one call site for "
                f"{description!r} in {SRC}, found {found}.\n"
                f"  devtools changed this call site; re-derive the patch before\n"
                f"  shipping, or builds will execute PKGBUILDs unconfined.",
                file=sys.stderr,
            )
            return 1
        text = text.replace(anchor, replacement, 1)

    os.makedirs(os.path.dirname(DST), exist_ok=True)

    # Written beside the target and renamed over it, never opened for writing in
    # place. `open(DST, "w")` truncates, and bash reads a script incrementally as
    # it runs -- so re-deriving this while a build was in flight rewrote the file
    # under the `makechrootpkg` executing it, mid-build. A rename swaps the
    # directory entry and leaves the running copy holding its own inode, which
    # is also what lets the package be upgraded while builds are running.
    tmp = f"{DST}.new"
    with open(tmp, "w", encoding="utf-8") as out:
        out.write(text)

    # A copy that is not executable fails the build with a confusing "permission
    # denied" rather than anything naming this file. Set the mode, then verify.
    os.chmod(tmp, 0o755)
    os.replace(tmp, DST)
    mode = os.stat(DST).st_mode
    if not mode & stat.S_IXUSR or not os.access(DST, os.X_OK):
        print(
            f"patch-makechrootpkg: {DST} is not executable ({mode & 0o777:o}).",
            file=sys.stderr,
        )
        return 1

    print(f"patch-makechrootpkg: applied {len(PATCHES)} patches to {DST}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
