# Uploading packages built elsewhere

Status: **Implemented** · Last updated: 2026-11-23

Some packages cannot be built where AURCache's workers run: `gcc13` or
`boost174-libs` run the build host out of memory, a large Rust project takes
longer than anyone wants to wait. Remote workers cover "build it on a bigger
machine" when that machine can run a worker. When it cannot -- a workstation
that runs `makechrootpkg` by hand, a one-off build -- the packages it produces
should still be published through AURCache, so clients keep one repository.

This lets an authenticated user upload the `.pkg.tar.*` files of one build of
a package AURCache already tracks, and publishes them through the same path a
worker's build takes.

## Requirements

1. **An upload is a build of an existing package.** An AUR or git package,
   whose own build may have failed here. An upload creates no package.
2. **An upload cannot publish anything the package's own build could not.**
   Its files are held to the names the package's source builds
   (`expected_pkgnames`), as a worker's artifacts are, and each file's
   contents must be what its name says.
3. **An upload is a build.** It gets a build row and a log, retires the
   previous version, and promotes the builds waiting on it, with the recovery
   a worker's build has.

## Validation

Before anything is recorded, every file is checked by its name and size; one
bad file refuses the upload with a 400 that names it.

- **Names**, as each part arrives and before its bytes are written: a bare
  `*.pkg.tar.zst` or `*.pkg.tar.xz`, or such a name plus `.sig`
  (`aurcache_common::upload::FileKind`). The package it names, or signs, must
  be one the source builds (`publish::artifact`), each package at most once,
  and at most 64 files. Refusing these before writing bounds what an upload
  holds on disk to one build.
- **One build:** every package the same version, every arch the same platform
  or `any`, and every `.sig` of a package in the upload. The platform is that
  arch; for an upload of only `any` files, the `platform` the request names, or
  the package's only platform. It must be one the package is configured for.
- **Sizes:** each file at most the package's `max_artifact_size`, a signature
  at most 16 KiB (what `repo-add` accepts), checked while streaming; over the
  limit is a 413. Multipart framing may take at most 8 MiB of the body, since
  the parser holds a part header in memory until it ends.

The `pkgbase` in the files is not checked: a package can be tracked under a
name that is not its pkgbase, and the names that its source builds limit what
it may publish just as tightly.

Publishing then reads each archive in full, as it reads a worker's: a
`.PKGINFO` whose `pkgname`, version or `arch` is not what the filename says
(`publish::check_contents`), or an ASCII-armored `.sig`, fails the build with
the reason in its log. makepkg's `-debug` packages are left out, as for a
worker's build. A signature is carried into the repository, not verified.

## Refusals

A refusal made before the body is read in full is answered once the rest of it
is read and dropped, up to 1 GiB. Otherwise hyper closes the connection on a
client still sending, which reports a broken pipe instead of the reason. That
includes a client that sent `Expect: 100-continue`: Rocket reads the start of
every body before routing, which already tells it to go on.

The auth guard's refusals are not drained, so nothing reads up to 1 GiB for a
client without a token: it sees the connection close rather than the 401.
`aurcli` asks about its token before sending anything.

## Flow

```
POST /api/package/<pkgbase>/upload[?platform=x86_64]
Content-Type: multipart/form-data   (one part per file, named by its filename)
```

1. The package is looked up (404 if there is none).
2. The body is streamed into a holding directory under the staging root
   (`.staging/upload-*`), each part checked as it arrives. The directory is
   removed when the request fails, and at startup if the process died
   mid-upload.
3. The files are checked as one build (above).
4. A build row goes straight into `PUBLISHING` with trigger `Upload` and no
   worker (`enqueue_build_if_missing` with `Pending::Publishing`). A build in
   progress for that package and platform refuses the upload with a 409. From
   here on the work runs in a task of its own, so a client that disconnects
   cannot leave a recorded build without its files.
5. The build log records who uploaded what, and the holding directory is
   renamed to the build's staging directory. A failed rename is logged, and
   publishing fails the build.
6. `publish_build` runs as for a worker's build. The request answers after
   that first attempt; a recoverable failure continues in the background
   through `publish_with_recovery`, and the answer says `PUBLISHING`.

The build row exists only once every file is held and checked, and a build is
`PUBLISHING` only with a complete staging directory or none. A restart between
4 and 5 finds nothing staged, and publishing fails the build ("no artifacts
were uploaded"). A restart after 5 resumes it like any interrupted publication.

## API, CLI and UI

- `UploadedBuild { pkgbase, number, platform, version, status }` is the
  response. `BuildTrigger::Upload` is a new value of an existing column, never
  sent to a worker. The activity log records `build.uploaded`.
- `aurcache-client`: `upload_package` streams files from disk;
  `upload_package_files` sends files already in memory.
- `aurcli pkg upload <pkgbase> <files>... [--platform <arch>]`. The pkgbase is
  named rather than read from the files, as a check that they are what the
  uploader meant to publish. Exits non-zero when publishing failed.
- UI: an Upload button in every package page's header, beside Rebuild. The
  browser reads the files into memory before sending them, so very large
  packages go through `aurcli`.

## Alternatives considered

- **One request per file, plus a "complete" call, as workers do.** Needs an
  upload session that lives between requests and expires, and a build that is
  half-uploaded meanwhile. A multipart request is all-or-nothing.
- **A build row created first, files uploaded into its staging directory.** A
  crash mid-upload would leave a `PUBLISHING` build with some of its files,
  which a restart would publish.
- **A new build state for "uploading".** Every consumer of `BuildState` would
  have to handle it, for a state no row needs.

## Non-goals

- Verifying signatures, or that the contents were built from the PKGBUILD.
  The uploader is an authenticated user; what is checked is that the upload
  cannot publish anything the package's own build could not.
- Bounding how many uploads run at once, or how long the server waits on a
  slow body. No other request has either.
