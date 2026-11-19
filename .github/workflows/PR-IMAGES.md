# Pull request images

A pull request's server image can be published to GHCR for testing, once a
maintainer labels the PR. Two workflows do this, so that PR code -- a fork's
included -- never runs where a token can push images.

## Build: `docker-pr-build.yml`

Every PR targeting master, forks included, builds without registry
credentials: a read-only token, no registry login, no shared build cache. It
is the only workflow that builds a PR's images; `docker-build.yml` runs on
pushes to master and on tags alone. The build runs `scripts/build-images.sh`,
the same script a release uses, for every architecture a release has, and its
`--oci-dir` output -- one OCI directory layout holding all of them -- is kept
as an Actions artifact for seven days. A new push cancels the build of the
previous one. Once a build's artifact has expired, re-running the build makes
a new one; each attempt of a run uploads its own, and the newest unexpired one
is what gets published.

Fork builds remain subject to GitHub's contributor approval policy.

## The `preview-image` label

Nothing is published for a PR without the `preview-image` label. While the
label is on, every successful build of the PR's current head is published:
new pushes and force-pushes included, with no need to re-apply it. Labelling a
PR therefore trusts every future push to its head branch, not just the commit
reviewed, until the label is removed -- and that is anyone who can push to the
branch, not only the PR's author: whoever the fork's owner lets push to it, or
anyone with write access for a branch of this repository. Removing the label
stops publishing. Only people with triage access can label a PR; do not let
automation apply this label.

## Publish: `docker-pr-publish.yml`

Runs from the default branch, on a fresh GitHub-hosted runner, when a PR build
finishes and when the label is added, so labelling publishes the head's
finished build at once. Either way it reads the current state from GitHub's
API and publishes only when:

- the PR is open against master and carries the label;
- the build is the latest one of the PR's current head commit from the PR's
  own branch, and it succeeded.

A build of a commit the PR has since moved on from is never published, nor
one superseded by a later build of the same commit. One publish runs at a time
per PR head branch.

The publisher downloads that build's artifact and never checks out PR code or
runs the image. Without the registry token, it checks the layout (only regular
files and directories, one small image index matching its digest) and
replaces the index's annotations with its own, from the API. Only then does a
step holding the token copy it with skopeo. The destination comes from the
API, not from the artifact.

Each commit is published as one multi-architecture image (amd64, arm64 and
armv7), so `docker pull` picks the host's:

- `ghcr.io/<owner>/<repo>-preview:pr-<number>-<full-head-sha>`, one per
  commit;
- `ghcr.io/<owner>/<repo>-preview:pr-<number>`, moved to the PR's latest
  published commit.

The package is separate from the release images; nothing in it is tagged
`latest`. After the push the publisher writes the image reference, commit and
index digest into one comment on the PR, edited in place for every later
commit, and into the run summary. Preview images contain untrusted PR code,
and are not deleted automatically.

## Which commit an image holds

The publisher replaces the annotations on each image's index with these, from
the API; none of the build's own index annotations survive:

- `org.opencontainers.image.revision`: the full head commit;
- `org.opencontainers.image.source`: this repository;
- `org.opencontainers.image.url`: the PR;
- `org.opencontainers.image.description`: the PR number and commit.

The PR and the commit are what identify the image, and both stay reachable
from this repository (the commit through the PR's `refs/pull/<number>/head`)
even after the fork is deleted. With the `pr-<number>-<sha>` tag, these are
what to trust:

```bash
docker buildx imagetools inspect ghcr.io/<owner>/<repo>-preview:pr-<number> \
  --format '{{json .Manifest.Annotations}}'
```

Everything else in the image comes from the PR's build -- the platform
manifests' annotations, the image labels, the version the server reports --
and is informational only.

## Setup

- The workflows must be on the default branch before the publisher can run.
- Create the `preview-image` label.
- After the first publish creates the preview package, make it public for
  anonymous pulls, and keep the repository's write access to it (package
  settings, Manage Actions access). No personal access token or deploy key is
  required.

## Keeping it safe

The publisher holds a token that can push packages. Keep it independent of
the build: do not add a checkout of PR code, PR scripts, shared build caches
or running the image to it. Download artifacts only from the build run it
selected, by id; check the layout's file types and sizes before parsing it,
in a step without the token, which only the login and copy step holds; and
derive destinations, PR numbers and annotations from GitHub's API, never from
artifact contents. Pin the actions it uses by commit.
