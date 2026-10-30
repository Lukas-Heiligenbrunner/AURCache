#!/usr/bin/env python3
"""Give a freshly populated showcase instance a year of build history.

Runs on the instance generate.sh assembled, with its server stopped: every
package has exactly one build, the demo worker's, of its current AUR version.
This turns that into what an instance that had been running for a while would
hold:

- Each package was added at some point in the first months of the window (a
  dependency with whatever needed it) and built whatever version the AUR had
  then; every version bump after that, read from the package's branch of the
  AUR git mirror, was picked up by the hourly version check and built. Bumps
  that landed within the same hour collapse into one build, as they would.
- A few of those builds failed and were retried by hand the next day. A few
  packages are failing right now: upstream moved on, the new version did not
  build, and the package sits out of date with the old version published --
  the state that waits for a human rather than for the scheduler, so a
  restored instance queues nothing.
- Durations, peak memory, disk use and artifact sizes follow from what kind of
  package it is (a repackaged binary, a Rust or Go program, a browser built
  from source), with per-build jitter.
- Every build gets a log in the shape a chroot worker writes, and the activity
  log gets the entries the server would have written along the way.

Everything is derived from seeded RNGs and the AUR history, so two runs over
the same mirror on the same day produce the same instance.
"""

import argparse
import json
import random
import re
import sqlite3
import subprocess
import time
import zlib
from datetime import datetime, timezone
from dataclasses import dataclass, field
from pathlib import Path

HOUR = 3600
DAY = 24 * HOUR
MIB = 1024 * 1024
GIB = 1024 * MIB

# BuildState / BuildTrigger, as stored (aurcache-common/src/build_state.rs).
SUCCESSFUL, FAILED = 1, 2
TRIGGER_USER, TRIGGER_AUTO = 0, 1

WORKERS = ["builder-01", "builder-02"]
# Who acts on the instance by hand: adding packages, retrying failures.
OPERATOR = "alice"
# How many packages are failing at the moment the snapshot is taken.
CURRENTLY_FAILING = 4
# Chance that a historical update build failed (and was retried).
FAILURE_RATE = 0.035


@dataclass
class Bump:
    when: int
    version: str


@dataclass
class Build:
    number: int
    version: str
    status: int
    start: int
    end: int
    trigger: int
    cause: str  # QueueCause: first, update, retry
    worker: str
    queued_by: str | None
    retried: int | None = None
    reason: str | None = None


@dataclass
class Package:
    id: int
    name: str
    requested: bool
    current: str  # the version the demo worker published
    demo_build_id: int
    srcinfo: str
    kind: str
    pkgnames: list[str]
    added_at: int = 0
    builds: list[Build] = field(default_factory=list)


def rng_for(*parts) -> random.Random:
    """A generator seeded by name, so each package's story is stable."""
    return random.Random(zlib.crc32("/".join(map(str, parts)).encode()))


def jitter(r: random.Random, base: int) -> int:
    return max(5, int(base * r.uniform(0.8, 1.25)))


# ---------------------------------------------------------------------------
# AUR history
# ---------------------------------------------------------------------------

LINE = re.compile(r"^([+-])\s*(pkgver|pkgrel|epoch) = (.*)$")


def aur_history(aur_git: Path, pkgbase: str) -> list[Bump]:
    """Every version the package's .SRCINFO declared, oldest first.

    Walks the diffs rather than reading the file at each commit: one `git log`
    per package instead of one `git show` per commit. The root commit adds
    every line, so the state is complete from the first entry on.
    """
    out = subprocess.run(
        ["git", "-C", str(aur_git), "log", "--reverse", "--format=@@ %ct", "-p",
         "--unified=0", f"origin/{pkgbase}", "--", ".SRCINFO"],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        return []
    bumps: list[Bump] = []
    state: dict[str, str] = {}
    when = None

    def flush():
        if when is None or "pkgver" not in state:
            return
        version = f"{state['pkgver']}-{state.get('pkgrel', '1')}"
        if state.get("epoch", "0") != "0":
            version = f"{state['epoch']}:{version}"
        if not bumps or bumps[-1].version != version:
            bumps.append(Bump(when, version))

    for line in out.stdout.splitlines():
        # A commit marker. A hunk header also starts with "@@ " but goes on
        # with "-", so the digits tell them apart.
        if line.startswith("@@ ") and line[3:].strip().isdigit():
            flush()
            when = int(line[3:])
            continue
        m = LINE.match(line)
        if not m:
            continue
        sign, key, value = m.group(1), m.group(2), m.group(3).strip()
        if sign == "+":
            state[key] = value
        elif state.get(key) == value:
            # Removed, and not re-added further down the same hunk (an epoch
            # dropped): the `+` line, when there is one, comes after.
            state.pop(key)
    flush()
    return bumps


# ---------------------------------------------------------------------------
# What kind of package, and what that makes its builds look like
# ---------------------------------------------------------------------------

HEAVY = re.compile(r"(chromium|^electron\d*$|firefox|thunderbird|^qt\d|webkit|llvm|"
                   r"^gcc|^linux-|libreoffice|blender)")


def classify(name: str, srcinfo: str) -> str:
    if name.endswith("-bin") or re.search(r"\.(deb|rpm|AppImage)\b", srcinfo):
        return "binary"
    if HEAVY.search(name):
        return "heavy"
    if re.search(r"makedepends = (cargo|rust)\b", srcinfo):
        return "rust"
    if re.search(r"makedepends = go\b", srcinfo):
        return "go"
    if re.search(r"makedepends = (cmake|meson|ninja)\b", srcinfo):
        return "native"
    if re.search(r"makedepends = python-(build|installer|setuptools)\b", srcinfo):
        return "python"
    if name.startswith(("ttf-", "otf-")) or "firmware" in name:
        return "data"
    return "plain"


# (duration seconds, artifact bytes, peak memory bytes) per kind, each a range
# a package's typical value is drawn from.
PROFILES = {
    "binary": ((25, 150), (40 * MIB, 260 * MIB), (300 * MIB, 900 * MIB)),
    "heavy": ((50 * 60, 4 * HOUR), (60 * MIB, 300 * MIB), (6 * GIB, 28 * GIB)),
    "rust": ((90, 14 * 60), (3 * MIB, 40 * MIB), (900 * MIB, 5 * GIB)),
    "go": ((40, 5 * 60), (4 * MIB, 45 * MIB), (500 * MIB, 3 * GIB)),
    "native": ((60, 20 * 60), (1 * MIB, 60 * MIB), (400 * MIB, 4 * GIB)),
    "python": ((20, 120), (200 * 1024, 15 * MIB), (150 * MIB, 600 * MIB)),
    "data": ((15, 90), (2 * MIB, 400 * MIB), (100 * MIB, 300 * MIB)),
    "plain": ((15, 6 * 60), (20 * 1024, 8 * MIB), (100 * MIB, 900 * MIB)),
}


@dataclass
class Shape:
    """One package's typical build, which each of its builds then jitters."""
    duration: int
    size: int
    memory: int
    chroot: int
    sources: int


def shape_of(pkg: Package) -> Shape:
    r = rng_for("shape", pkg.name)
    (d0, d1), (s0, s1), (m0, m1) = PROFILES[pkg.kind]

    def pick(lo, hi):
        # Log-uniform: sizes and times spread over orders of magnitude.
        return int(lo * (hi / lo) ** r.random())

    size = pick(s0, s1)
    sources = (int(size * r.uniform(0.6, 1.3)) if pkg.kind in ("binary", "data")
               else pick(200 * 1024, 300 * MIB))
    return Shape(duration=pick(d0, d1), size=size, memory=pick(m0, m1),
                 chroot=int(r.uniform(1.6, 4.2) * GIB), sources=sources)


# ---------------------------------------------------------------------------
# The timeline
# ---------------------------------------------------------------------------

FAILURE_KINDS = ["build", "checksum", "deps", "source"]


def added_at(pkg: Package, history: list[Bump], window_start: int, now: int) -> int:
    """When the package was added: in the first months, most of them early --
    the instance was set up, then grew. A package newer than that was added
    soon after it appeared."""
    r = rng_for("added", pkg.name)
    t = window_start + int((r.random() ** 2) * 120 * DAY) + r.randint(0, DAY)
    if history and history[0].when > t:
        t = history[0].when + r.randint(2 * DAY, 20 * DAY)
    return min(t, now - 3 * DAY)


def plan(pkg: Package, history: list[Bump], now: int) -> None:
    """The package's builds from when it was added up to its current version."""
    r = rng_for("plan", pkg.name)

    before = [b for b in history if b.when <= pkg.added_at]
    versions: list[tuple[int, str, str]] = [  # (when, version, cause)
        (pkg.added_at, before[-1].version if before else pkg.current, "first")]
    for bump in (b for b in history if b.when > pkg.added_at):
        # The hourly version check noticed, and a worker took the build.
        when = bump.when + r.randint(4 * 60, 70 * 60)
        if when >= now - HOUR:
            break
        if when - versions[-1][0] < HOUR:
            versions[-1] = (versions[-1][0], bump.version, versions[-1][2])
        elif bump.version != versions[-1][1]:
            versions.append((when, bump.version, "update"))
    # The last build is the version the demo worker published, so a mirror
    # that lags or leads the AUR is lined up with it.
    built = [v for _, v, _ in versions]
    if pkg.current in built:
        versions = versions[: len(built) - built[::-1].index(pkg.current)]
    else:
        when = max(versions[-1][0] + DAY, now - r.randint(2 * DAY, 9 * DAY))
        versions.append((min(when, now - 2 * HOUR), pkg.current, "update"))

    shape = shape_of(pkg)
    number = 0
    for i, (when, version, cause) in enumerate(versions):
        worker = WORKERS[r.random() < 0.35]
        retried = None
        if i < len(versions) - 1 and cause == "update" and r.random() < FAILURE_RATE:
            number += 1
            pkg.builds.append(Build(
                number, version, FAILED, when, when + jitter(r, shape.duration // 3),
                TRIGGER_AUTO, cause, worker, None, reason=r.choice(FAILURE_KINDS)))
            # Retried by hand, the next working day.
            when += r.randint(14 * HOUR, 40 * HOUR)
            cause, retried = "retry", number
        number += 1
        pkg.builds.append(Build(
            number, version, SUCCESSFUL, when, when + jitter(r, shape.duration),
            TRIGGER_AUTO if cause == "update" else TRIGGER_USER, cause, worker,
            OPERATOR if cause in ("first", "retry") and pkg.requested else None,
            retried=retried))


def bump_pkgrel(version: str) -> str:
    """The next pkgrel: an upstream fix to the PKGBUILD."""
    head, _, rel = version.rpartition("-")
    try:
        return f"{head}-{int(rel.split('.')[0]) + 1}"
    except ValueError:
        return f"{version}.1"


# ---------------------------------------------------------------------------
# Logs
# ---------------------------------------------------------------------------

def stamp(t: int) -> str:
    """A date as makepkg prints it -- left as a placeholder, because the
    snapshot is restored later than it was taken and restore.py moves every
    time forward by the difference, these included."""
    return f"@@DATE:{t}@@"


def sources_of(srcinfo: str) -> list[tuple[str, str]]:
    """(file name, url) for each source, as makepkg names them."""
    out = []
    for m in re.finditer(r"^\s*source(?:_x86_64)? = (.*)$", srcinfo, re.M):
        src = m.group(1).strip()
        name, _, url = src.partition("::")
        if not url:
            url, name = name, name.rsplit("/", 1)[-1]
        out.append((name, url))
    return out[:6] or [("PKGBUILD", "")]


GO_MODULES = [
    "github.com/spf13/cobra v1.10.2", "github.com/spf13/pflag v1.0.10",
    "golang.org/x/sys v0.38.0", "golang.org/x/term v0.37.0", "golang.org/x/text v0.31.0",
    "github.com/leonelquinteros/gotext v1.7.2", "github.com/Jguer/go-alpm/v2 v2.3.0",
    "github.com/Morganamilo/go-srcinfo v1.0.0", "github.com/adrg/strutil v0.3.1",
    "github.com/hashicorp/go-multierror v1.1.1", "gopkg.in/yaml.v3 v3.0.1",
    "github.com/mattn/go-isatty v0.0.20", "github.com/stretchr/testify v1.11.1",
    "github.com/ohler55/ojg v1.26.10", "github.com/deckarep/golang-set/v2 v2.8.0",
    "github.com/itchyny/gojq v0.12.17", "github.com/klauspost/compress v1.18.1",
    "github.com/google/uuid v1.6.0", "github.com/fsnotify/fsnotify v1.9.0",
    "github.com/sirupsen/logrus v1.9.3",
]

C_WORDS = ["main", "config", "util", "log", "buffer", "parser", "lexer", "render",
           "window", "input", "audio", "video", "net", "socket", "thread", "pool",
           "hash", "list", "map", "string", "file", "path", "image", "font", "theme",
           "event", "timer", "signal", "plugin", "module", "cache", "db", "json", "xml"]


def build_output(pkg: Package, build: Build, r: random.Random, crates: list[str]) -> list[str]:
    """What build() printed."""
    upstream = build.version.rsplit("-", 1)[0].split(":")[-1]
    if pkg.kind == "rust":
        lines = [f"   Compiling {r.choice(crates)}" for _ in range(r.randint(140, 320))]
        took = build.end - build.start
        return lines + [f"   Compiling {pkg.name} v{upstream} (/build/{pkg.name}/src/{pkg.name}-{upstream})",
                        f"    Finished `release` profile [optimized] target(s) in {took // 60}m {took % 60:02d}s"]
    if pkg.kind == "go":
        return ([f"go: downloading {m}" for m in r.sample(GO_MODULES, k=r.randint(8, 20))]
                + ["go build -trimpath -buildmode=pie -mod=readonly -modcacherw -o build/ ."])
    if pkg.kind == "python":
        return ["* Getting build dependencies for wheel...", "* Building wheel...",
                f"Successfully built {pkg.name.removeprefix('python-')}-{upstream}-py3-none-any.whl"]
    if pkg.kind in ("binary", "data"):
        return []
    total = r.randint(60, 260)
    files = r.sample(C_WORDS, k=24)
    return (["-- The C compiler identification is GNU 15.2.1",
             "-- Detecting C compiler ABI info - done",
             f"-- Configuring done ({r.uniform(0.5, 4):.1f}s)",
             f"-- Generating done ({r.uniform(0.05, 0.4):.1f}s)",
             f"-- Build files have been written to: /build/{pkg.name}/src/build"]
            + [f"[{i}/{total}] Building C object CMakeFiles/{pkg.name}.dir/src/{files[i % 24]}.c.o"
               for i in range(1, total)]
            + [f"[{total}/{total}] Linking C executable {pkg.name}"])


def write_log(path: Path, pkg: Package, build: Build, crates: list[str]) -> None:
    """A log in the shape a chroot worker writes; see the TrueNAS logs it is
    modelled on in the commit that added this."""
    r = rng_for("log", pkg.name, build.number)
    sources = sources_of(pkg.srcinfo)
    out = ["[worker] preparing chroot", "[worker] starting build", "[worker] downloading source",
           ":: Synchronizing package databases...", " core downloading...",
           " extra downloading...", " multilib downloading...", " repo downloading...",
           ":: Starting full system upgrade...", " there is nothing to do",
           f"==> Making package: {pkg.name} {build.version} ({stamp(build.start)})",
           "==> Checking runtime dependencies...", "==> Checking buildtime dependencies...",
           "==> Retrieving sources..."]
    out += [f"  -> Downloading {name}..." if url.startswith(("http", "ftp")) else f"  -> Found {name}"
            for name, url in sources]
    failure = build.reason if build.status == FAILED else None

    def failed(*lines: str) -> None:
        out.extend(lines)
        out.append(f"==> ERROR: Build failed, check /var/lib/aurcache-chroot/job-{r.randint(1000, 9999)}/build")
        out.append("worker reported failure: build failed (exit 255)")
        save(path, out)

    if failure == "source":
        return failed(f"==> ERROR: Failure while downloading {sources[0][1] or sources[0][0]}",
                      "    Aborting...")
    out.append("==> Validating source files with sha256sums...")
    if failure == "checksum":
        return failed(f"    {sources[0][0]} ... FAILED",
                      "==> ERROR: One or more files did not pass the validity check!")
    out += [f"    {name} ... Passed" for name, _ in sources]
    if failure == "deps":
        return failed("==> Installing missing dependencies...",
                      "==> ERROR: 'pacman' failed to install missing dependencies.",
                      "==> ERROR: Could not resolve all dependencies.")
    out.append("==> Extracting sources...")
    out += [f"  -> Extracting {name} with bsdtar" for name, _ in sources
            if re.search(r"\.(tar\.\w+|tgz|zip|deb)$", name)]
    if pkg.kind not in ("binary", "data") or failure == "build":
        out.append("==> Starting build()...")
        lines = build_output(pkg, build, r, crates)
        if failure == "build":
            out += lines[: len(lines) // 2]
            return failed(f"error: could not compile `{pkg.name}` (bin \"{pkg.name}\") due to 2 previous errors"
                          if pkg.kind == "rust" else "make: *** [Makefile:42: all] Error 1",
                          "==> ERROR: A failure occurred in build().", "    Aborting...")
        out += lines
    out += ["==> Entering fakeroot environment...", "==> Starting package()...",
            "==> Tidying install...", "  -> Removing libtool files...",
            "  -> Purging unwanted files...", "  -> Removing static library files...",
            "  -> Stripping unneeded symbols from binaries and libraries...",
            "  -> Compressing man and info pages...", "==> Checking for packaging issues..."]
    for name in pkg.pkgnames:
        out += [f"==> Creating package \"{name}\"...", "  -> Generating .PKGINFO file...",
                "  -> Generating .BUILDINFO file...", "  -> Generating .MTREE file...",
                "  -> Compressing package..."]
    out += ["==> Leaving fakeroot environment.",
            f"==> Finished making: {pkg.name} {build.version} ({stamp(build.end)})"]
    out += [f"[worker] uploading {name}-{build.version}-x86_64.pkg.tar.zst" for name in pkg.pkgnames]
    out.append("Published to the repository")
    save(path, out)


@dataclass
class RealBuild:
    """A build that actually ran (generate.sh's e2e step), as the API
    reported it, which stands in for a package's latest build."""
    start: int
    duration: int
    size: int | None
    peak_memory: int | None
    disk: dict
    files: dict[str, int]
    log: str


def load_real(root: Path) -> dict[str, RealBuild]:
    out = {}
    for d in sorted(root.iterdir()) if root.is_dir() else []:
        build = json.loads((d / "build.json").read_text())
        package = json.loads((d / "package.json").read_text())
        out[d.name] = RealBuild(
            start=build["start_time"], duration=build["end_time"] - build["start_time"],
            size=build.get("size"), peak_memory=build.get("peak_memory"),
            disk=build.get("disk_usage") or {},
            files={f["filename"]: f["size"] for f in package.get("files", []) if f.get("size")},
            log=(d / "output.log").read_text())
    return out


MAKEPKG_DATE = re.compile(r"\((\w{3} \w{3} +\d{1,2} \d\d:\d\d:\d\d \d{4})\)")


def real_log(real: RealBuild, start: int) -> str:
    """The real log, its makepkg dates turned into placeholders moved to
    when this build happened instead."""
    def placeholder(m: re.Match) -> str:
        when = datetime.strptime(" ".join(m.group(1).split()), "%a %b %d %H:%M:%S %Y")
        t = int(when.replace(tzinfo=timezone.utc).timestamp())
        return f"({stamp(t - real.start + start)})"
    return MAKEPKG_DATE.sub(placeholder, real.log)


def save(path: Path, lines: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(lines) + "\n")


def log_path(root: Path, pkgbase: str, number: int) -> Path:
    """aurcache_common::fs::build_log_path: <root>/<sanitized pkgbase>/<number>.log"""
    safe = re.sub(r"[^A-Za-z0-9\-_.+@]", "_", pkgbase)
    if safe in ("", ".", ".."):
        safe = "_" + safe
    return root / safe / f"{number}.log"


# ---------------------------------------------------------------------------
# The activity log
# ---------------------------------------------------------------------------

class Activity:
    """Writes log rows and their entity index the way the server does: the
    payload holds `ns:id` references, and each one is indexed under the field
    (role) it appeared in, a build also under its package."""

    def __init__(self, db: sqlite3.Connection):
        self.db = db

    def add(self, when: int, kind: str, severity: int, message: str, data: dict,
            refs: list[tuple[str, str]], user: str | None = None) -> None:
        cur = self.db.execute(
            "INSERT INTO log (kind, severity, message, data, timestamp, user) VALUES (?,?,?,?,?,?)",
            (kind, severity, message, json.dumps(data, separators=(",", ":"), sort_keys=True),
             when, user))
        for role, ref in refs:
            ns, ident = ref.split(":", 1)
            entities = [(ns, ident)]
            if ns == "build":
                entities.append(("pkg", ident.rsplit("/", 1)[0]))
            for ns, ident in entities:
                self.db.execute("INSERT OR IGNORE INTO log_entity (log_id, role, ns, id) VALUES (?,?,?,?)",
                                (cur.lastrowid, role, ns, ident))


def record(act: Activity, pkg: Package) -> None:
    name, pkg_ref = pkg.name, f"pkg:{pkg.name}"
    act.add(pkg.added_at - 30, "package.added", 0, f"added package {name}", {"pkg": pkg_ref},
            [("pkg", pkg_ref)], OPERATOR if pkg.requested else None)
    built = None
    for b in pkg.builds:
        ref = f"build:{name}/{b.number}"
        queued = {"builds": [ref], "cause": b.cause, "pkg": pkg_ref}
        refs = [("builds", ref), ("pkg", pkg_ref)]
        if b.cause == "first":
            message = f"queued {name} #{b.number} (first build: {b.version})"
            queued["version"] = b.version
        elif b.cause == "update":
            message = f"queued {name} #{b.number} (new version {b.version})"
            queued["version"] = b.version
            act.add(b.start - 40, "version.detected", 0,
                    f"new version {b.version} detected for {name} (built: {built})",
                    {"built": built, "pkg": pkg_ref, "vcs": False, "version": b.version},
                    [("pkg", pkg_ref)])
        else:
            retried = f"build:{name}/{b.retried}"
            message = f"queued {name} #{b.number} (retry of {name} #{b.retried})"
            queued["retried"] = retried
            refs.append(("retried", retried))
        act.add(b.start - 20, "build.queued", 0, message, queued, refs, b.queued_by)
        worker = f"worker:{b.worker}"
        pair = {"build": ref, "worker": worker}
        act.add(b.start, "build.started", 0, f"{b.worker} picked up {name} #{b.number}", pair,
                [("build", ref), ("worker", worker)])
        if b.status == SUCCESSFUL:
            act.add(b.end, "build.succeeded", 0, f"{name} #{b.number} finished on {b.worker}", pair,
                    [("build", ref), ("worker", worker)])
            act.add(b.end + 2, "build.published", 0, f"published {name} #{b.number}: {b.version}",
                    {"build": ref, "version": b.version}, [("build", ref)])
            built = b.version
        else:
            reason = "build failed (exit 255)"
            act.add(b.end, "build.failed", 1, f"{name} #{b.number} failed on {b.worker}: {reason}",
                    {**pair, "reason": reason}, [("build", ref), ("worker", worker)])


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--instance", type=Path, required=True)
    ap.add_argument("--aur-git", type=Path, required=True)
    ap.add_argument("--months", type=int, default=12)
    ap.add_argument("--real", type=Path, help="builds that actually ran, from generate.sh")
    args = ap.parse_args()

    inst = args.instance
    db = sqlite3.connect(inst / "db" / "db.sqlite")
    db.execute("PRAGMA foreign_keys = ON")
    now = int(time.time())
    window_start = now - args.months * 30 * DAY

    # Crate names for the Rust logs: this project's dependencies, the ones
    # from crates.io -- not its own crates, which no other package compiles.
    lock = (Path(__file__).resolve().parents[2] / "backend" / "Cargo.lock").read_text()
    crates = [f"{n} v{v}" for n, v in re.findall(
        r'name = "([^"]+)"\nversion = "([^"]+)"\nsource = "registry\+', lock)]

    packages: list[Package] = []
    for pid, name, requested, split, build_id, version in db.execute("""
            SELECT p.id, p.name, p.directly_requested, p.split_packages, b.id, b.version
            FROM packages p JOIN builds b ON b.pkg_id = p.id
            WHERE b.status = 1 ORDER BY p.id"""):
        path = inst / "source_cache" / f"aur_{name}" / ".SRCINFO"
        srcinfo = path.read_text() if path.exists() else ""
        packages.append(Package(
            id=pid, name=name, requested=bool(requested), current=version,
            demo_build_id=build_id, srcinfo=srcinfo, kind=classify(name, srcinfo),
            pkgnames=json.loads(split or "[]") or [name]))
    by_id = {p.id: p for p in packages}

    histories = {p.name: aur_history(args.aur_git, p.name) for p in packages}
    for pkg in packages:
        if pkg.requested:
            pkg.added_at = added_at(pkg, histories[pkg.name], window_start, now)
    # A dependency was added with the first package that needed it, a moment
    # before it. Chains are short; repeating settles them.
    edges = db.execute("SELECT dependent_id, dependee_id FROM dependencies").fetchall()
    for _ in range(5):
        for dependent, dependee in edges:
            d, e = by_id.get(dependent), by_id.get(dependee)
            if d and e and not e.requested and d.added_at:
                e.added_at = min(e.added_at or d.added_at, d.added_at - 60)
    for pkg in packages:
        if not pkg.added_at:
            pkg.added_at = added_at(pkg, histories[pkg.name], window_start, now)
        plan(pkg, histories[pkg.name], now)

    # A package built for real takes that build's duration for its latest.
    reals = load_real(args.real) if args.real else {}
    for pkg in packages:
        if pkg.name in reals:
            current = [b for b in pkg.builds if b.status == SUCCESSFUL][-1]
            current.end = current.start + reals[pkg.name].duration

    # The few packages failing right now: upstream moved on and the new
    # version did not build, so they stay out of date for a human to look at.
    failing = sorted((p for p in packages
                      if p.requested and p.kind != "data" and p.name not in reals),
                     key=lambda p: zlib.crc32(p.name.encode()))[:CURRENTLY_FAILING]
    for i, pkg in enumerate(failing):
        r = rng_for("failing", pkg.name)
        start = max(now - r.randint(6 * HOUR, 5 * DAY), pkg.builds[-1].end + HOUR)
        pkg.builds.append(Build(
            pkg.builds[-1].number + 1, bump_pkgrel(pkg.current), FAILED, start,
            min(now - 60, start + jitter(r, shape_of(pkg).duration // 2)), TRIGGER_AUTO,
            "update", WORKERS[i % 2], None, reason=FAILURE_KINDS[i % len(FAILURE_KINDS)]))

    # The fleet: two chroot workers, as a small real instance would have.
    db.execute("""UPDATE workers SET kind = 'chroot', version = '0.6.0', concurrency = 2,
                  last_seen = ? WHERE name = ?""", (now - 20, WORKERS[0]))
    if not db.execute("SELECT 1 FROM workers WHERE name = ?", (WORKERS[1],)).fetchone():
        db.execute("""INSERT INTO workers (name, status, cert_fingerprint, signed_cert, not_after,
                      arches, last_seen, version, package_affinity, priority,
                      concurrency, kind, settings_declaration, effective_config, paused)
                      SELECT ?, status, 'showcase-' || cert_fingerprint, signed_cert, not_after,
                      arches, ?, version, package_affinity, priority,
                      concurrency, kind, settings_declaration, effective_config, paused
                      FROM workers WHERE name = ?""", (WORKERS[1], now - 35, WORKERS[0]))
    worker_ids = dict(db.execute("SELECT name, id FROM workers"))

    # Out with the demo's own records, in with the history.
    db.execute("DELETE FROM log")
    db.execute("DELETE FROM activity")
    act = Activity(db)
    for pkg in packages:
        db.execute("DELETE FROM builds WHERE pkg_id = ? AND id != ?", (pkg.id, pkg.demo_build_id))
        # Out of the way of the history's numbers until it takes its own.
        db.execute("UPDATE builds SET number = -1 WHERE id = ?", (pkg.demo_build_id,))
        shape = shape_of(pkg)
        current = [b for b in pkg.builds if b.status == SUCCESSFUL][-1]
        for b in pkg.builds:
            r = rng_for("build", pkg.name, b.number)
            size = int(shape.size * r.uniform(0.97, 1.05)) if b.status == SUCCESSFUL else None
            values = dict(
                status=b.status, start_time=b.start, end_time=b.end, platform="x86_64",
                version=b.version, worker_id=worker_ids[b.worker], number=b.number, size=size,
                peak_memory=int(shape.memory * r.uniform(0.85, 1.15)), trigger=b.trigger,
                disk_chroot=int(shape.chroot * r.uniform(0.98, 1.03)),
                disk_workdir=int((size or shape.size) * r.uniform(1.8, 3.5)),
                disk_sources=int(shape.sources * r.uniform(0.98, 1.02)))
            real = reals.get(pkg.name) if b is current else None
            if real:
                values.update(
                    size=real.size, peak_memory=real.peak_memory,
                    disk_chroot=real.disk.get("chroot"), disk_workdir=real.disk.get("workdir"),
                    disk_sources=real.disk.get("sources"), disk_build_tree=real.disk.get("build_tree"))
            if b is current:
                # The demo worker's build becomes this one: it is what holds
                # the published artifacts.
                db.execute(f"UPDATE builds SET {', '.join(f'{k} = :{k}' for k in values)} WHERE id = :id",
                           {**values, "id": pkg.demo_build_id})
            else:
                db.execute(f"INSERT INTO builds (pkg_id, {', '.join(values)}) "
                           f"VALUES (:pkg, {', '.join(f':{k}' for k in values)})",
                           {**values, "pkg": pkg.id})
            path = log_path(inst / "build_logs", pkg.name, b.number)
            if real:
                save(path, real_log(real, b.start).rstrip("\n").split("\n"))
            else:
                write_log(path, pkg, b, crates)
        record(act, pkg)

        # The artifact sizes the dashboard sums.
        files = db.execute("SELECT id, filename FROM files WHERE package_id = ?", (pkg.id,)).fetchall()
        real_files = reals[pkg.name].files if pkg.name in reals else {}
        for fid, filename in files:
            size = real_files.get(filename) or max(1024, current_size(db, pkg) // len(files))
            db.execute("UPDATE files SET size = ? WHERE id = ?", (size, fid))

        last = pkg.builds[-1]
        db.execute("UPDATE packages SET status = ?, out_of_date = ?, upstream_version = ? WHERE id = ?",
                   (last.status, int(last.status == FAILED), last.version, pkg.id))

    first_add = min(p.added_at for p in packages)
    for i, w in enumerate(WORKERS):
        t = first_add - DAY + i * HOUR
        ref = f"worker:{w}"
        act.add(t, "worker.enrolled", 0, f"worker {w} enrolled", {"worker": ref}, [("worker", ref)])
        act.add(t + 300, "worker.approved", 0, f"approved worker {w}", {"worker": ref},
                [("worker", ref)], OPERATOR)
    # Deploys: the server restarted every few weeks.
    r = rng_for("deploys")
    t = first_add - DAY - HOUR
    while t < now:
        act.add(t, "server.start", 0, "AURCache 0.6.0 started", {"version": "0.6.0"}, [])
        t += r.randint(9 * DAY, 30 * DAY)

    db.commit()
    total, failed = db.execute("SELECT count(*), sum(status = 2) FROM builds").fetchone()
    print(f"history: {len(packages)} packages, {total} builds ({failed} failed), failing now: "
          + ", ".join(p.name for p in failing))


def current_size(db: sqlite3.Connection, pkg: Package) -> int:
    return db.execute("SELECT size FROM builds WHERE id = ?", (pkg.demo_build_id,)).fetchone()[0]


if __name__ == "__main__":
    main()
