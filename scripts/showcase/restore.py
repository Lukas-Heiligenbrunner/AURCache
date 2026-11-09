#!/usr/bin/env python3
"""Restore the showcase snapshot and run a server on it.

    ./scripts/showcase/restore.py              # build, restore, serve on :8080
    ./scripts/showcase/restore.py --no-serve   # only lay the instance out

The snapshot (scripts/showcase/snapshot/, made by generate.sh) is an instance
frozen at the moment it was taken. Restoring it moves every recorded time
forward by however long ago that was, so the instance reads as if it had been
running until a minute ago: last night's builds are last night's, the monthly
graph ends this month, and the workers last checked in just now.

The server runs offline as far as the AUR goes, so what it shows is the
snapshot and nothing else:

- The AUR RPC is answered from the snapshot's own database (`AUR_RPC_URL`), so
  the version check the server runs at startup finds exactly the upstream
  versions that were recorded -- no package drifts out of date because the AUR
  moved on since. Anything else asked of the RPC (the add dialog's search) is
  passed through to the real AUR.
- The packages' AUR repositories are local git repositories made from the
  snapshot's copies of their files (`AUR_GIT_URL`).

The mirrorlist and the official repositories' databases still come from the
network, as on any instance.
"""

import argparse
import http.server
import json
import os
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tarfile
import threading
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

PROJECT = Path(__file__).resolve().parents[2]
SNAPSHOT = PROJECT / "scripts" / "showcase" / "snapshot"
SERVER = PROJECT / "backend" / "target" / "debug" / "aurcache"

# Every column holding a point in time, in epoch seconds.
TIME_COLUMNS = {
    "builds": ["start_time", "end_time", "lease_expires_at", "kept_until"],
    "log": ["timestamp"],
    "activity": ["timestamp"],
    "workers": ["last_seen", "not_after"],
    "operations": ["created_at", "finished_at"],
    "package_vcs_sources": ["updated_at"],
    "packages": ["source_first_submitted", "source_last_modified"],
}

DATE = re.compile(r"@@DATE:(\d+)@@")


def restore(snapshot: Path, target: Path) -> None:
    if target.exists():
        shutil.rmtree(target)
    (target / "db").mkdir(parents=True)

    dump = subprocess.run(["zstd", "-dc", snapshot / "db.sql.zst"], check=True,
                          capture_output=True).stdout
    subprocess.run(["sqlite3", target / "db" / "db.sqlite"], input=dump, check=True)
    with subprocess.Popen(["zstd", "-dc", snapshot / "files.tar.zst"],
                          stdout=subprocess.PIPE) as unzstd:
        with tarfile.open(fileobj=unzstd.stdout, mode="r|") as tar:
            tar.extractall(target, filter="data")

    taken = int((snapshot / "taken-at").read_text())
    delta = int(time.time()) - taken
    shift(target / "db" / "db.sqlite", delta)
    for log in (target / "build_logs").rglob("*.log"):
        text = log.read_text()
        log.write_text(DATE.sub(lambda m: makepkg_date(int(m.group(1)) + delta), text))
    for pkg in sorted((target / "aur").iterdir()):
        repository(pkg, target / "aur-git" / f"{pkg.name}.git")
    print(f"restored to {target}, {delta // 86400} days after the snapshot was taken")


def shift(db_path: Path, delta: int) -> None:
    db = sqlite3.connect(db_path)
    for table, columns in TIME_COLUMNS.items():
        sets = ", ".join(f"{c} = {c} + :d" for c in columns)
        db.execute(f"UPDATE {table} SET {sets}", {"d": delta})
    db.commit()


def makepkg_date(t: int) -> str:
    return datetime.fromtimestamp(t, timezone.utc).strftime("%a %b %d %H:%M:%S %Y")


def repository(files: Path, bare: Path) -> None:
    """A repository whose HEAD is the package's files, as the AUR serves it."""
    stamp = f"{int(time.time())} +0000"
    git = ["git", f"--git-dir={bare}", f"--work-tree={files}",
           "-c", "user.name=AURCache showcase", "-c", "user.email=showcase@example.invalid",
           "-c", "core.autocrlf=false", "-c", "core.safecrlf=false"]
    subprocess.run(["git", "init", "-q", "--bare", "-b", "master", bare], check=True)
    # Forced: some AUR repositories ignore `*` and add their files by hand.
    subprocess.run([*git, "add", "-A", "-f"], check=True)
    subprocess.run([*git, "commit", "-q", "--allow-empty", "-m", "Snapshot"], check=True,
                   env={**os.environ, "GIT_AUTHOR_DATE": stamp, "GIT_COMMITTER_DATE": stamp})


class Rpc(http.server.BaseHTTPRequestHandler):
    """The AUR RPC, answered from the restored database for `info` and passed
    through to the AUR for anything else."""

    packages: dict[str, dict] = {}

    def do_GET(self) -> None:
        url = urllib.parse.urlsplit(self.path)
        if url.path.rstrip("/").endswith("/info"):
            names = urllib.parse.parse_qs(url.query).get("arg[]", [])
            results = [self.packages[n] for n in dict.fromkeys(names) if n in self.packages]
            return self.answer(200, {"version": 5, "type": "multiinfo",
                                     "resultcount": len(results), "results": results})
        try:
            with urllib.request.urlopen(f"https://aur.archlinux.org{self.path}", timeout=30) as up:
                self.answer(up.status, json.load(up))
        except Exception as e:  # noqa: BLE001 -- any failure is the RPC's error
            self.answer(502, {"type": "error", "error": str(e), "results": []})

    def answer(self, status: int, body: dict) -> None:
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_args) -> None:
        pass

    @classmethod
    def load(cls, db_path: Path) -> None:
        db = sqlite3.connect(db_path)
        for (name, split, version, desc, maintainer, url, licenses, first, last) in db.execute("""
                SELECT name, split_packages, upstream_version, source_description,
                       source_maintainer, source_project_url, source_licenses,
                       source_first_submitted, source_last_modified
                FROM packages WHERE source_data LIKE '{"type":"aur"%'"""):
            for pkgname in json.loads(split or "[]") or [name]:
                cls.packages[pkgname] = {
                    "Name": pkgname, "PackageBase": name, "Version": version or "",
                    "Description": desc, "Maintainer": maintainer, "URL": url,
                    "License": licenses.split(", ") if licenses else None,
                    "FirstSubmitted": first or 0, "LastModified": last or 0,
                    "OutOfDate": None, "NumVotes": 0, "Popularity": 0,
                }


def serve(target: Path, server: Path, rpc_port: int) -> None:
    Rpc.load(target / "db" / "db.sqlite")
    rpc = http.server.ThreadingHTTPServer(("127.0.0.1", rpc_port), Rpc)
    threading.Thread(target=rpc.serve_forever, daemon=True).start()

    env = {
        **os.environ,
        "AUR_RPC_URL": f"http://127.0.0.1:{rpc_port}/rpc/v5",
        "AUR_GIT_URL": f"file://{target / 'aur-git'}",
        # A year of history, kept whole rather than cut to the default 90 days.
        "ACTIVITY_RETENTION": "0",
        # Nothing connects as the workers; they last checked in when the
        # snapshot was taken, which the shift made "just now".
        "WORKER_LIVENESS_TIMEOUT": "86400",
    }
    with open(target / "server.log", "ab") as log:
        proc = subprocess.Popen([server], cwd=target, env=env, stdout=log, stderr=log)
    signal.signal(signal.SIGTERM, lambda *_: proc.terminate())
    try:
        for _ in range(120):
            if proc.poll() is not None:
                sys.exit(f"the server exited; see {target / 'server.log'}")
            try:
                urllib.request.urlopen("http://localhost:8080/api/packages/list?limit=1", timeout=2)
                break
            except OSError:
                time.sleep(1)
        print("serving on http://localhost:8080 (Ctrl-C to stop)", flush=True)
        proc.wait()
    except KeyboardInterrupt:
        pass
    finally:
        proc.terminate()
        proc.wait()
        rpc.shutdown()


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--snapshot", type=Path, default=SNAPSHOT)
    ap.add_argument("--dir", type=Path,
                    default=Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
                    / "aurcache" / "showcase-restored")
    ap.add_argument("--server", type=Path, help="a server binary to run instead of building one")
    ap.add_argument("--rpc-port", type=int, default=8099)
    ap.add_argument("--no-serve", action="store_true", help="restore, then exit")
    args = ap.parse_args()

    restore(args.snapshot, args.dir)
    if args.no_serve:
        return
    server = args.server
    if server is None:
        subprocess.run(["cargo", "build", "--quiet", "-p", "aurcache", "--features",
                        "aurcache-api/static"], cwd=PROJECT / "backend", check=True)
        server = SERVER
    serve(args.dir, server, args.rpc_port)


if __name__ == "__main__":
    main()
