# Uploading packages built elsewhere

Status: **Implemented** · Last updated: 2026-11-21

Left out on purpose: packages that are only ever uploaded (see "Follow-up"),
signature verification (under "Non-goals"), and a platform picker in the
browser upload (an upload of only `arch = any` packages to a package built for
several platforms goes through `aurcli pkg upload --platform`).

Some packages cannot be built where AURCache's workers run: `gcc11` or
`boost174-libs` run the build host out of memory, a large Rust project takes
longer than anyone wants to wait. Remote workers cover "build it on a bigger
machine" when that machine can run a worker. When it cannot -- a workstation
that runs `makechrootpkg` by hand, a one-off build -- the packages it produces
should still be published through AURCache, so clients keep one repository.

This lets an authenticated user upload the `.pkg.tar.*` files of one build of
a package AURCache already tracks, and publishes them through the same path a
worker's build takes.

---

## Requirements

1. **An upload is a build of an existing package.** An AUR or git package,
   whose own build may have failed here: that is the case the request came
   from. An upload creates no package.
2. **An upload cannot publish anything the package's own build could not.**
   Its files are held to the names the package's source builds
   (`expected_pkgnames`), as a worker's artifacts are, and each file's
   contents must be what its name says.
3. **An upload is a build.** It gets a build row and a log, goes through the
   states a build goes through, retires the previous version, and promotes the
   builds waiting on it, with the recovery semantics a worker's build has.

---

## Design

### What an upload may publish

The names its package's source builds: `expected_pkgnames`, the row's name
plus `split_packages` from the `.SRCINFO` -- the rule `publish::artifact`
already holds a worker's artifacts to, and the one the upload asks. An upload
cannot add a name the source does not build, so it claims nothing new and no
other package's names need checking here; publishing's own check that no
other package publishes a name the build does still applies, as for any
build.

The files' `pkgbase` is not checked: a package can be tracked under a name
that is not its pkgbase, and the names its source builds bound what it may
publish as tightly.

After an upload, the next upstream version is built as usual. If that build
fails, the uploaded version stays in the repository (a failed build retires
nothing), and another upload replaces it.

An upload is a whole build, with what that means for a build: split packages
it leaves out are retired on that platform, and its version replaces what was
there even when it is older.

### Validation

Before anything is recorded, every file is checked; one bad file refuses the
upload, with a 400 that names it:

- **Names:** a bare filename (no `/`, not dot-prefixed), `*.pkg.tar.zst` or
  `*.pkg.tar.xz` (what `repo.db` entries can be made from), or such a name plus
  `.sig` (`aurcache_common::upload::FileKind`), checked as each part arrives,
  before its bytes are read. The package it names, its own or the one it
  signs, must be one the source builds (`publish::artifact`), and its version
  `[epoch:]pkgver-pkgrel` as makepkg writes one (digits for the epoch, no
  whitespace, `/`, `:` or `-` in the pkgver, `N` or `N.M` for the pkgrel). At
  most 64 files, none twice. The name, count and duplicate rules are
  `aurcache_common::upload::Names`, which the server takes each arriving
  part's name through and `aurcli` its files' names before it sends anything
  (`check_names`).
  A `.sig` must belong to a package in the same upload. makepkg's `-debug`
  package of one the source builds is taken but not read, and publishing
  leaves it out with a line in the build log, as for a worker's build
  (`Artifact::Debug`): the repository never carries one, and `makechrootpkg`
  writes them by default.
- **Contents:** each package's `.PKGINFO` is read
  (`pacman_repo_utils::read_package_meta`). makepkg writes it after
  `.BUILDINFO` and `.MTREE` and before the package's files, so this reads a
  small part of the archive, and at most 64 MiB of it decompressed; the whole
  archive is read once, later, when publishing, within the bounds archive
  reading has for every build. `pkgname`, version and `arch` must be what the
  filename says (`publish::check_contents`, which publishing applies to a
  worker's artifacts too).
- **One build:** every file has the same version, and every file's `arch` is
  either the same platform or `any`. The platform is that arch; for an upload
  of only `any` files it is the `platform` the request names, or the package's
  only platform.
- **Platform:** one the package is configured for.
- **Sizes:** each file at most the package's `max_artifact_size` (the setting
  a worker's artifacts are held to, per package), a signature at most 16 KiB
  (what `repo-add` accepts), checked while streaming so nothing over the limit
  is ever fully written; a file over its limit is a 413. The multipart framing
  -- boundaries and part headers, and what the parser reads ahead -- may come
  to at most 8 MiB of the body: the parser holds a part header in memory until
  it ends, so one that never ends would otherwise grow the server until it is
  killed. Between those and the count of files the body as a whole is
  bounded, so it has no limit of its own.
- **Signatures:** a detached `.sig` is carried into the repository beside its
  package and into the `repo.db` entry, as for a worker's build. It is checked
  when the upload arrives with what publishing checks
  (`pacman_repo_utils::read_signature`): binary, as `repo-add` requires, not
  ASCII-armored, and at most 16 KiB. It is not verified: the server holds no
  keyring, and the client that installs the package checks it against its
  own.

### Flow

```
POST /api/package/<pkgbase>/upload[?platform=x86_64]
Content-Type: multipart/form-data   (one part per file, named by its filename)
```

1. The package is looked up (404 if there is none), before any of the body
   is read. The body is streamed into a holding directory under the staging
   root (`.staging/upload-*`), each part checked for its name and size as it
   arrives. The directory is removed if the request fails at any point, and at
   startup if the process died mid-upload. A refusal before the body is read in
   full is answered once the rest of it is read and dropped, up to 1 GiB, so
   that a client still sending reads the refusal rather than a broken
   connection; one with more than that left to send is cut off, and may see
   only the broken connection. That holds for a client that sent `Expect:
   100-continue` too: Rocket reads the start of every request's body before
   routing it, which tells such a client to go on, so it is sending by the
   time any refusal is made. Whether a build is in the way is decided in step
   3 only, once the files are in.
2. The files are validated (above).
3. One transaction: a build row straight in `PUBLISHING` with trigger `Upload`
   and no worker (`build_enqueue::record_upload`), and
   `refresh_package_status`. A build already in progress for that package and
   platform (the partial unique index) refuses the upload with a 409: cancel
   it, or wait. The package row is read again in this transaction, locked; one
   removed meanwhile is a 409 too. From this step on the work runs in a task of
   its own, so a client that disconnects cannot leave a recorded build without
   its files.
4. The holding directory is renamed to the build's staging directory -- the one
   a worker's uploads land in -- and the build log records who uploaded what.
5. Publishing runs as for a worker's build (`publish_build`): the filename and
   content checks again, the check that no other package publishes one of its
   names, the repository update through `Repository`, retiring the files the
   build no longer produces, `PUBLISHING -> SUCCESSFUL`, and
   `trigger_dependents`. The request waits for that first attempt and answers
   with the build's number and state; a recoverable failure continues in the
   background through `publish_with_recovery`, and the answer says
   `PUBLISHING`.

Crash-safety follows from the order: the build row exists only once every file
is staged and valid, and a build is `PUBLISHING` only with a complete staging
directory or none. A restart between 3 and 4 finds a `PUBLISHING` build with
nothing staged, and publishing fails it ("no artifacts were uploaded"). A
restart after 4 resumes it like any interrupted publication.

### Who uploaded

`Authenticated`, like every write endpoint. The build log opens with
`Uploaded by <user>` (or `Uploaded`, on an instance without sign-in) and the
files it carried, and the activity log records `build.uploaded` by that user.

### API, CLI and UI

- `aurcache-common`: `UploadedBuild { pkgbase, number, platform, version,
  status }` is the response. `BuildTrigger::Upload` records why the row
  exists; it is a new value of an existing integer column, never sent to a
  worker. A listed build carries `uploaded` in the API (`BuildSummary`;
  neither `aurcli` nor the UI shows it). Its times are when it arrived and when
  it was published, so the average and longest build times and the package
  page's typical duration leave it out.
- `aurcache-client`: `upload_package(pkgbase, platform, paths)` streams files
  from disk (native only: a browser has no paths); `upload_package_files` sends
  files already in memory, which is what the browser has.
- `aurcli pkg upload <pkgbase> <files>... [--platform <arch>]`. The pkgbase is
  named rather than read from the files: it is the check that the files are
  what the uploader meant to publish. The file names are checked before
  anything is sent. Exits non-zero when publishing failed, pointing at the
  build's log.
- UI: every package page has an Upload button in its header, beside Rebuild.
  The browser reads the files into the page before sending them, so very large
  packages go through `aurcli`.

---

## Follow-up: packages that are only ever uploaded

A package with no source here at all -- one nothing could build, published
only from its uploads -- is a separate change. It needs a source kind no
worker ever builds, guards on every queueing path, and names that come from
its uploads rather than a `.SRCINFO`, which then have to be kept from
colliding with every other package's. None of that is needed while every
upload is held to a source's names.

## Alternatives considered

- **One request per file, as workers do, plus a "complete" call.** Needs an
  upload session that lives between requests (and expires), and a build that
  is half-uploaded while it does. A multipart request is all-or-nothing, and a
  browser's `FormData` and reqwest's `multipart` both stream it.
- **A build row created first, files uploaded into its staging directory.** A
  crash mid-upload would leave a `PUBLISHING` build with only some of its
  files, which a restart would publish. The holding directory is what keeps
  "staged" meaning "complete".
- **A new build state for "uploading".** Every consumer of `BuildState` would
  have to decide about it, for a state no row needs: the row is created
  complete.

## Non-goals

- Verifying signatures, or that the contents were built from the PKGBUILD.
  The uploader is an authenticated user; what is checked is that the upload
  cannot publish anything the package's own build could not.
- Bounding how many uploads run at once, or how long the server waits on a
  slow body: there is no upload concurrency limit and no server-side read
  timeout, and no other request has either. An authenticated client that
  holds connections open, or sends slowly, holds a holding directory and a
  connection for as long as it does.
