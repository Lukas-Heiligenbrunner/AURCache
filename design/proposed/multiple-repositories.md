# Several repositories, with different packages

Status: **Proposed** · Last updated: 2026-11-24

An instance publishes one pacman repository, and every package is in it.
Machines with different roles want different packages: a `desktop`
repository for laptops and a `server` repository for headless machines,
with some packages in both. This lets an instance publish several
repositories, each listing a chosen set of packages.

A package lists the repositories that it is in, and nothing else. A repository
lists the platforms that it publishes. On each platform a package is built
once and published to every repository that lists it. Building packages
another way (`x86_64_v3`, LTO, another distribution) is `build-targets.md`,
where a target is one more thing that a repository can publish.

---

## Where things stand

- **One repository.** `Repository` (`aurcache-utils/src/repository.rs`) is
  documented as "one per server". It has a root, a name
  (`AURCACHE_REPO_NAME`, default `repo`), a lock, and the signing key. Its
  layout is `<root>/<arch>/<name>.db`, and uploads are staged under
  `<root>/.staging/`.
- **Publishing links, then renames.** `stage_file` hard-links a staged
  upload to a `.publishing` sibling beside it in staging (copying when a
  link is impossible) and keeps the upload until publication finishes.
  Finalization renames the link into the platform directory. `commit`
  refuses a file staged outside its own root.
- **Settling.** `publish_build` skips a build that is already `recorded`
  (its `size` is set, which the commit does) and goes on to settle it:
  remove its staging and promote its dependents.
- **One recovery plan.** `repository_updates` holds the one pending plan,
  which startup, the next update and the sweep replay.
- **The sweep** walks every subdirectory of the root, and judges by
  `repo.db`.
- **Files.** `files (filename, platform, package_id, size)` records what the
  repository lists. An orphaned row keeps its file listed in `repo.db`, so
  rows must never be orphaned.
- **Serving.** nginx (compose) and Rocket's file server serve any path under
  the root, so another directory under it is served with no new code.
- **Builds use AURCache's own repository** for dependencies, through
  `RegisterStatus::repo_template`, which reads the first (and only)
  repository.
- **Quarantine** (`package-suspicion-signals.md`, proposed) adds a second
  `Repository` under `<root>/quarantine/<arch>/` for held builds.

## Design

### Repositories

A `repositories` table: `id` and `name`. The repository with id 1 always
exists and is the one that exists today. Its name still comes from
`AURCACHE_REPO_NAME` when that is set.

- **Layout.** The first repository keeps `<root>/<arch>/`, so the `Server =`
  line on every existing client keeps working. Each other repository is at
  `<root>/<name>/<arch>/`, and clients reach it at `AURCACHE_PUBLIC_URL`
  plus `/<name>`. `validate_name` also refuses the names of platforms and
  targets, and `quarantine`, because they would collide with directories
  under the root. Dot names, such as `.staging`, are refused already.
- **One `Repository` per repository.** `Services` holds a map from id to
  `Repository`, each with its own lock. They share the instance's staging
  root, so `commit` accepts a file staged there rather than only under its
  own root. Each repository has its own `resign/` directory under it, since
  `stage_published` empties the one that it uses, without the lock.
  `repository_updates` gains `repository_id`, one pending plan per
  repository. The first repository's sweep skips the other repositories'
  directories. The instance's one key signs every repository.
- **`files` gains `repository_id`.** The same file in two repositories is two
  rows, and each repository's sweep judges by its own `repo.db`.
  `package_delete` removes the package from every repository.

### Membership

A `package_repositories (package_id, repository_id)` table. A new package
joins the first repository, so an instance with one repository behaves as
it does now. The add page and the package page have a checkbox per
repository.

- **Dependencies follow.** Adding a package to a repository also adds its
  dependencies there, so that a client of that repository can install it. A
  dependency leaves a repository only when nothing in that repository needs
  it any more, which `live_check` decides for the whole instance today.
  Removing a requested package from a repository where another package
  still needs it keeps it there as a dependency.
- **Joining a repository links the package's current builds in**, from a
  repository that publishes them, through `stage_published`
  (`repository.rs`), the path that a backfill uses. The `.sig` beside each
  file is linked with it, or the file is signed as a backfill signs it:
  finalization removes a published signature that has no staged one. A
  dependent waits for that as it waits for a publish. A package with no
  current build on one of the repository's platforms gets a build queued.
- **Leaving a repository retires the package's files there**, with the
  usual grace period. A package in no repository stays listed in AURCache
  but has no platforms, so it is not built and is published nowhere.

### Platforms belong to repositories

A package lists one thing: its repositories. Each repository lists the
platforms that it publishes (the targets of `build-targets.md`, once those
exist), and a package is built for every platform of its repositories that
its PKGBUILD's `arch` allows (an `arch=(any)` package on a target is the
exception; see `build-targets.md`). `packages.platforms` goes away, along with
the platform chooser on the package page.

So a package that should not be built for `aarch64` goes in a repository
without `aarch64`, and a package with `arch=(x86_64)` is never built for
`aarch64`, whatever its repositories publish. The migration gives the first
repository the union of the platforms that packages list today. A package
whose list was narrower then starts building for more platforms, and the
migration logs each such package.

### Builds

A package has one build per platform, whichever repositories list it, so a
package in two repositories that publish the same platform is built once.

A build installs its dependencies from the first repository, through
`repo_template`. So at first, the first repository lists every package:
otherwise a dependency of a package that is only in `server` could not be
installed while building. Once workers can be told which directory to read
(the job's repository, in the worker protocol that `build-targets.md`
adds), a build reads, by name and directory, one of its package's
repositories that publishes the build's platform. Dependencies follow a
package into each repository, so that repository has every dependency. The
first repository then lists only its own packages, and builds of packages
outside it go only to workers on that protocol.

### Publishing once to several repositories

A finished build is staged once, under `<root>/.staging/<build>/`, and
committed to each repository that lists the package:

- **Hard links.** Each repository links its files from that one staging
  directory, as `stage_file` already does for the first, and the staged
  upload is removed once every repository has published it. Published files
  are never changed in place, and signatures are separate `.sig` files, so a
  change in one repository cannot reach another through a link. Retiring a
  file in one repository removes only that repository's link. Every
  repository must be on the root's filesystem, since the link is made in
  staging and renamed in. Each repository's link has a name of its own
  (`.publishing-<repository id>`), so that one repository's pending
  finalization does not block another's.
- **One repository at a time.** Each repository's commit runs under its own
  lock, one after the other, so no publish holds two locks. Each commit's
  transaction also writes a `published_to (build_id, repository_id)` row,
  and a retry skips the repositories that have one. That check replaces
  `recorded`, which cannot tell repositories apart, because the first commit
  sets the build's size. Checking for `files` rows would not work either,
  since a rebuild at the same version reuses them. The build is settled only
  after the last repository, so an interrupted publish resumes as it does
  today.
- **Sizes.** Each repository's size is the sum of its files. The
  instance's disk use counts each build's artifacts once, however many
  repositories list them, since links share their blocks.

### API and CLI

- `GET /repositories` lists the repositories: name, platforms, how each is
  published, and its contents (package count, and per platform the packages
  listed and their size), which the Repository page reads once there are
  several. `GET /repo/info` stays, and answers for the first repository.
- Routes to create, rename and delete a repository, and a package's
  repositories as a field on the package.
- `aurcli repo config --repo <name>` prints one repository's stanza, and
  `aurcli repo list` lists them.
- The Repository page (`repository-page.md`) shows one set of cards per
  repository. The Packages list can filter by repository.

## What this does not do

- **Different packages under one name.** A package name is unique in the
  instance, so one repository cannot hold a fork of `foo` while another
  holds the upstream `foo`.
- **Per-repository signing keys.** One key signs every repository.
- **Restrict who reads which repository.** That is a question for the
  mirror rather than for this design.

## Open questions

- **Quarantine per repository.** Is one `quarantine` enough, given that
  builds are shared?
- **Packages that gain platforms.** Is it acceptable that the migration
  builds a package for more platforms than it listed? The alternative is to
  create a repository for each distinct platform list in use, which would
  give existing clients repositories that they never asked for.

## Delivery

1. **Repositories without a second one.** `repositories` with one row and
   its platforms, replacing `packages.platforms`, `files.repository_id`,
   `repository_updates.repository_id`, the map of `Repository` instances
   and `GET /repositories`. The only visible change is that platforms are
   chosen on the repository. Quarantine and `build-targets.md` can build on
   this step.
2. **Further repositories.** `package_repositories`, publishing with hard
   links, dependencies following, and the UI and CLI to manage them. The
   first repository lists every package: its checkbox is shown checked and
   cannot be cleared.
3. **Builds read their own repository**, with the job's repository from
   `build-targets.md`. The first repository then lists only its own
   packages, and its checkbox can be cleared.

## Alternatives

- **Copies instead of hard links.** Simplest, and it doubles the storage for
  every extra repository.
- **reflinks** (`cp --reflink`, ZFS block cloning). These give independent
  files that share blocks, which only matters if a file changes in place.
  Published files never do, and reflinks need a filesystem that supports
  them.
- **Symbolic links to the first repository's files.** No new inode, but
  nginx and Rocket would follow them out of the repository's directory. A
  link would also break silently if the first repository retired the file
  first.
- **A recovery plan that spans repositories**, with every lock taken in a
  fixed order. It makes a multi-repository publish one atomic step, which
  nothing needs: each repository is consistent on its own.
- **Separate instances.** These share no builds, which is the point of this
  design. `independent-repositories.md` compares them with one instance for
  repositories that differ in both packages and build rules.
