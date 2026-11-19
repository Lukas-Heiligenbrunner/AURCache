#!/usr/bin/env python3
"""Patch `makechrootpkg` for the worker: confinement, and one nspawn default.

Mostly the first. The `arch-nspawn` wrapper patch is not about confinement
and says so; it opts out of a systemd-nspawn default that is about to change.
The three after it close the other direction: links the build leaves in its
output directories, which `makechrootpkg` would otherwise `chown` through as
root on the host. The upgrade patch keeps one build's output from reaching
into the next, and installs the build's dependencies with no network; the
two after it leave the build no way back to root: see their comments.

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

Writes AURCache's private copy to /usr/lib/aurcache/bin/aurcache-build. The
worker invokes it by absolute path; the packaged script is left alone. The copy
is named for what it is to AURCache rather than for what it was derived from,
and it takes a leading `--interface=N` the stock script rejects: so the stock
script can be neither mistaken for it by name nor run in its place by argv.

Every patch MUST apply exactly once. If devtools moves a call site this fails
the image build rather than silently shipping a worker that executes PKGBUILDs
unconfined — the reason a patch is preferred here over shadowing `makepkg` on
PATH, whose failure mode would be invisible.
"""

import os
import stat
import sys

SRC = os.environ.get("MAKECHROOTPKG_SRC", "/usr/bin/makechrootpkg")
# A location AURCache owns, named by the worker as an absolute path
# (`aurcache_worker::build::SCRIPT`).
#
# Not `/usr/local/bin`: that belongs to the administrator rather than to a
# package, and is on sudo's `secure_path` -- the worker runs this through sudo,
# which replaces PATH -- so nothing would stop an unqualified name finding
# something else. Naming the file absolutely removes that question entirely.
DST = os.environ.get("AURCACHE_BUILD_DST", "/usr/lib/aurcache/bin/aurcache-build")

# The interface the copy speaks, as `aurcache_worker::build::INTERFACE` (a unit
# test there checks the two agree). Bump both when the worker starts relying on
# something a copy derived by the previous version of this file would not do.
INTERFACE = "3"


def _no_follow(description: str, var: str) -> tuple[str, str, str]:
    """Make one of the loops that hand outputs back skip anything that is not a
    regular file, and `chown -h` the rest; `var` is the loop's variable. See
    the comment above its uses in PATCHES."""
    return (
        description,
        f'\t\tchown "$src_owner" "${var}"\n',
        f"\t\tif [[ -L ${var} || ! -f ${var} ]]; then\n"
        f"\t\t\twarning 'Skipping %s: not a regular file' \"${{{var}##*/}}\"\n"
        "\t\t\tcontinue\n"
        "\t\tfi\n"
        f'\t\tchown -h "$src_owner" "${var}"\n',
    )


# (description, anchor, replacement)
PATCHES = [
    (
        # The worker's own interface, ahead of makechrootpkg's options. Stock
        # `makechrootpkg` parses its options with getopts, which answers
        # `--interface...` with usage and exit 1 -- so if the stock script is
        # ever run where this copy should be, the build stops before anything
        # executes, rather than running with the PKGBUILD unconfined. The worker
        # asks `--interface` at startup and refuses to start on any other
        # answer, which covers a missing copy too.
        #
        # Required rather than optional, or the stock script's answer to a
        # build would be the only thing telling them apart. Before getopts, and
        # before `check_root`: the query needs no root, and an unprivileged
        # caller's re-exec gets the argument back (see below).
        "the build interface: a leading argument stock makechrootpkg rejects",
        "\nwhile getopts '",
        f"""
case ${{1:-}} in
\t--interface)
\t\tprintf '%s\\n' {INTERFACE}
\t\texit 0
\t\t;;
\t--interface={INTERFACE})
\t\tshift
\t\t;;
\t*)
\t\tdie 'aurcache-build speaks interface %s and needs --interface=%s first, not %s' \\
\t\t\t{INTERFACE} {INTERFACE} "${{1:-nothing}}"
\t\t;;
esac

while getopts '""",
    ),
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
        # The re-exec passes on `"$@"`, from which the interface patch above
        # has shifted `--interface=N`, so it is put back in front.
        "AURCACHE_* and the interface kept across the root re-exec",
        "check_root SOURCE_DATE_EPOCH,BUILDTOOL,BUILDTOOLVER,GNUPGHOME,SRCDEST,"
        'SRCPKGDEST,PKGDEST,LOGDEST,NPROC,MAKEFLAGS,PACKAGER "${BASH_SOURCE[0]}" "$@"',
        "check_root SOURCE_DATE_EPOCH,BUILDTOOL,BUILDTOOLVER,GNUPGHOME,SRCDEST,"
        "SRCPKGDEST,PKGDEST,LOGDEST,NPROC,MAKEFLAGS,PACKAGER,AURCACHE_DROPIN,"
        "AURCACHE_NSPAWN_KEEP_UNIT,AURCACHE_PACMAN_CONF,AURCACHE_UPGRADE_CONF,"
        "AURCACHE_BUILD_DEPS "
        f'"${{BASH_SOURCE[0]}}" --interface={INTERFACE} "$@"',
    ),
    (
        # `-u` runs `pacman -Syuu` in the copy with every repository its
        # pacman.conf lists -- for the worker, AURCache's own among them. An
        # upgrade is where pacman acts on `replaces=`, and `--noconfirm`
        # accepts the replacement. pacman looks for an installed package's
        # replacement in each repository in turn, stopping at the first that
        # has it by name, and AURCache's comes last: what it can replace is an
        # installed package no official repository has any more, which a base
        # chroot refreshed in place keeps. A package AURCache built that
        # declares `replaces=` for one would be installed as root into every
        # later build, including builds that do not depend on it, and so into
        # their output.
        #
        # So `-u` is refused, and the worker hands over two configurations
        # instead (`aurcache_worker::devtools::PacmanConfs`): the build's, with
        # AURCache's repository, and the upgrade's, the same without it. Both
        # are installed into the copy; every database the build's lists is
        # synced; the system is upgraded reading only those the upgrade's
        # lists, since pacman loads the databases a configuration names and no
        # others; and the build installs its dependencies with the build's,
        # through `pacman -S`, which ignores `replaces=`. One sync rather than
        # one per configuration, so the build installs from exactly the
        # databases the system was upgraded against.
        #
        # `-uu`, as devtools does: with only the operator's repositories in
        # the configuration, a downgrade can only be to what those serve.
        #
        # Either step failing fails the build. A system left un-upgraded
        # against databases already synced is a partial upgrade, which the
        # dependency install would then build on.
        #
        # Neither variable set (anyone running this copy by hand) skips the
        # sync, as `makechrootpkg` without `-u` does.
        #
        # Then the build's dependencies, which makepkg would install itself
        # through `sudo pacman`: as root, with the build's network, running
        # other packages' `.install` scriptlets and alpm hooks. The worker
        # names them in `AURCACHE_BUILD_DEPS` (`aurcache_worker::build_deps`),
        # one per line as makepkg would pass them, and they are installed
        # here, against the databases just synced, in two steps: downloaded
        # with network, which runs nothing from a package, then installed
        # under `aurcache-sandbox offline`, which leaves the transaction and
        # everything it starts no network: a namespace of its own, a filter
        # allowing UNIX sockets only, or both. The sandbox is bound
        # into the container read-only; it is the worker's own, of the same
        # architecture as the chroot. Only what the copy does not already
        # satisfy, as makepkg asks `pacman -T` -- otherwise a name an
        # installed package provides could pull in a second provider. Unset,
        # nothing is installed, and the build (which can no longer become
        # root, below) fails on anything missing.
        "-u refused; the copy synced, then upgraded without AURCache's repository",
        '(( update_first )) && arch-nspawn "$copydir" \\\n'
        '\t\t--machine="$(machine_name upgrade-packages)" \\\n'
        '\t\t"${bindmounts_ro[@]}" "${bindmounts_rw[@]}" "${bindmounts_tmpfs[@]}" \\\n'
        "\t\tpacman -Syuu --noconfirm\n",
        """(( update_first )) &&
\tdie 'aurcache-build does not take -u, which upgrades with every repository the chroot lists; set AURCACHE_PACMAN_CONF and AURCACHE_UPGRADE_CONF instead'

if [[ -n ${AURCACHE_PACMAN_CONF:-} || -n ${AURCACHE_UPGRADE_CONF:-} ]]; then
\t[[ -n ${AURCACHE_PACMAN_CONF:-} && -n ${AURCACHE_UPGRADE_CONF:-} ]] ||
\t\tdie 'AURCACHE_PACMAN_CONF and AURCACHE_UPGRADE_CONF are set together or not at all'
\tinstall -Dm644 "$AURCACHE_PACMAN_CONF" "$copydir/etc/pacman.conf" ||
\t\tdie "Unable to install %s" "$AURCACHE_PACMAN_CONF"
\tinstall -Dm644 "$AURCACHE_UPGRADE_CONF" "$copydir/etc/aurcache/pacman-upgrade.conf" ||
\t\tdie "Unable to install %s" "$AURCACHE_UPGRADE_CONF"
\tarch-nspawn "$copydir" \\
\t\t--machine="$(machine_name sync-databases)" \\
\t\t"${bindmounts_ro[@]}" "${bindmounts_rw[@]}" "${bindmounts_tmpfs[@]}" \\
\t\tpacman -Sy --noconfirm ||
\t\tdie "Could not sync the package databases."
\tarch-nspawn "$copydir" \\
\t\t--machine="$(machine_name upgrade-packages)" \\
\t\t"${bindmounts_ro[@]}" "${bindmounts_rw[@]}" "${bindmounts_tmpfs[@]}" \\
\t\tpacman -Suu --noconfirm --config /etc/aurcache/pacman-upgrade.conf ||
\t\tdie "Could not upgrade the chroot."
fi

if [[ -n ${AURCACHE_BUILD_DEPS:-} ]]; then
\tmapfile -t _aurcache_deps <"$AURCACHE_BUILD_DEPS" ||
\t\tdie "Unable to read %s" "$AURCACHE_BUILD_DEPS"
\t_aurcache_missing=()
\tif (( ${#_aurcache_deps[@]} )); then
\t\t_aurcache_out=$(pacman --dbpath "$copydir/var/lib/pacman" -T -- "${_aurcache_deps[@]}")
\t\tcase $? in
\t\t\t0) ;;
\t\t\t127) mapfile -t _aurcache_missing <<<"$_aurcache_out" ;;
\t\t\t*) die "Could not check the build's dependencies." ;;
\t\tesac
\tfi
\tif (( ${#_aurcache_missing[@]} )); then
\t\t_aurcache_sandbox=$(command -v aurcache-sandbox) ||
\t\t\tdie "aurcache-sandbox is needed to install the build's dependencies"
\t\tarch-nspawn "$copydir" \\
\t\t\t--machine="$(machine_name download-dependencies)" \\
\t\t\t"${bindmounts_ro[@]}" "${bindmounts_rw[@]}" "${bindmounts_tmpfs[@]}" \\
\t\t\tpacman -Sw --noconfirm -- "${_aurcache_missing[@]}" ||
\t\t\tdie "Could not download the build's dependencies."
\t\tarch-nspawn "$copydir" \\
\t\t\t--machine="$(machine_name install-dependencies)" \\
\t\t\t--bind-ro="${_aurcache_sandbox//:/\\\\:}:/run/aurcache-sandbox" \\
\t\t\t"${bindmounts_ro[@]}" "${bindmounts_rw[@]}" "${bindmounts_tmpfs[@]}" \\
\t\t\t/run/aurcache-sandbox offline -- \\
\t\t\tpacman -S --asdeps --needed --noconfirm -- "${_aurcache_missing[@]}" ||
\t\t\tdie "Could not install the build's dependencies."
\tfi
fi
""",
    ),
    (
        # devtools lets the build user run pacman as root, for makepkg to
        # install the dependencies it finds missing -- and so lets the build
        # itself become root in its chroot whenever it likes
        # (`sudo pacman -U` of a package it made, with a scriptlet). The
        # dependencies are installed before the build instead (the upgrade
        # patch above), so the entry goes. The copy is made from a base that
        # never had one, but one a copy was left holding goes with it.
        "no sudoers entry letting the build user run pacman as root",
        """\tcat > "$copydir/etc/sudoers.d/builduser-pacman" <<EOF
builduser ALL = NOPASSWD: /usr/bin/pacman
EOF
\tchmod 440 "$copydir/etc/sudoers.d/builduser-pacman"
""",
        """\trm -f "$copydir/etc/sudoers.d/builduser-pacman"
""",
    ),
    (
        # And with no sudoers entry, no setuid program either: the build's
        # container runs with `no_new_privs`, so sudo, su and every file
        # capability are inert for the build user. Root dropping to that
        # user, which is how `/chrootbuild` starts makepkg, is a `setuid()`
        # rather than a setuid exec, and is unaffected.
        "the build runs with no_new_privs",
        "nspawn_build_args=(\n",
        "nspawn_build_args=(\n\t--no-new-privileges=yes\n",
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
    _no_follow("move_logfiles: never follow a link the build left in logdest", "l"),
    _no_follow("move_products: never follow a link the build left in pkgdest", "pkgfile"),
    _no_follow("move_products: never follow a link the build left in srcpkgdest", "s"),
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
    # under the copy executing it, mid-build. A rename swaps the
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
