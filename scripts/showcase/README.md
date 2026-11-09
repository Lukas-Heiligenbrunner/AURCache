# Showcase instance

A populated AURCache instance, kept as a snapshot so it can be brought up in
seconds: the AUR's most popular packages (with their dependencies) and about a
year of build history. It is what the website's screenshots are taken from
(`scripts/update-website-screenshots.sh`), and handy whenever a screen needs
looking at with realistic data behind it.

```bash
./scripts/showcase/restore.py      # build the server, restore, serve on :8080
```

## What is real and what is not

Real: the package list (the most popular AUR packages with at least 50 votes,
VCS packages left out), their metadata, dependencies and AUR files, and the
dates of every version bump, which come from each package's history in the
AUR's git mirror.

Made up: everything about the builds. Nothing was compiled; the published
archives are the demo worker's near-empty ones. Durations, sizes, memory and
disk use are drawn from what kind of package it is, a few builds fail along the
way, and four packages are failing now. The logs follow the shape of a real
chroot worker's. See `history.py` for how.

## Files

- `snapshot/db.sql.zst` -- the database, as an SQLite dump. It records its
  migrations, so a snapshot taken today still restores after the schema moves
  on: the server migrates it at startup like any other database.
- `snapshot/files.tar.zst` -- the repository, the build logs, and each
  package's AUR files.
- `snapshot/taken-at` -- when it was taken. Restoring moves every time in the
  instance forward by the time since, so it always reads as current.
- `generate.sh` -- makes a new snapshot (about fifteen minutes; needs network
  and a clone of the AUR's git mirror, https://github.com/archlinux/aur).
- `history.py` -- the history part of that, run on the freshly populated
  instance.
- `restore.py` -- lays a snapshot out and serves it. The server reaches neither
  the AUR's RPC nor its git: both are answered from the snapshot
  (`AUR_RPC_URL`, `AUR_GIT_URL`), so a restored instance shows exactly what
  was saved, however long ago.
