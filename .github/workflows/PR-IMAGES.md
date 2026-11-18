# Pull request images

A pull request's server image can be published to GHCR for testing, once a
maintainer approves it. Three workflows do this, so that PR code -- a fork's
included -- never runs where a token can push images.

## Build: `docker-pr-build.yml`

Every PR targeting master, forks included, builds without registry
credentials: a read-only token, no registry login, no shared build cache. It
is the only workflow that builds a PR's images; `docker-build.yml` runs on
pushes to master and on tags alone. The build runs `scripts/build-images.sh`,
the same script a release uses, for every architecture a release has, and its
`--oci-dir` output -- one OCI directory layout holding all of them -- is kept
as an Actions artifact for seven days. A new push cancels the build of the
previous one.

Fork builds remain subject to GitHub's contributor approval policy.

## Approval: the `preview-image` label

Nothing is published until a maintainer adds the `preview-image` label. The
label approves the commit that is the PR's head when it is applied, and no
later push: a commit pushed after the label, or pushed again after it, is
published only once a maintainer removes and re-applies it. Only people with
triage access can label a PR; do not let automation apply this label.

## Publish: `docker-pr-publish.yml`

Runs from the default branch, on a fresh GitHub-hosted runner, when a PR build
finishes and when the label is added. Either way it reads the current state
from GitHub's API and publishes only when:

- the PR is open against master;
- it carries the label;
- the latest build of its head commit, from the PR's own branch, succeeded;
- the label was applied after that build was created.

It downloads that build's artifact and copies the layout with skopeo; it never
checks out PR code or runs the image. The destination comes from the API, not
from the artifact. One publish runs at a time per PR head branch.

Each commit is published as one multi-architecture image (amd64, arm64 and
armv7), so `docker pull` picks the host's:

- `ghcr.io/<owner>/<repo>-preview:pr-<number>-<full-head-sha>`

The package is separate from the release images and has no mutable tag, no
`latest` and no `pr-<number>`. After the push the publisher writes the image
reference into one comment on the PR, edited in place for every later commit,
and into the run summary. Preview images contain untrusted PR code.

## Cleanup: `docker-pr-cleanup.yml`

Deletes a PR's images when it closes, and every day deletes those of closed
PRs and any published more than 30 days ago, along with the untagged platform
and attestation versions each image lists.

## Setup

- The workflows must be on the default branch before the publisher and the
  cleanup can run.
- Create the `preview-image` label.
- After the first publish creates the preview package, make it public for
  anonymous pulls. The repository needs write access to it to publish and the
  Admin role to delete versions (package settings, Manage Actions access).
  No personal access token or deploy key is required.

## Keeping it safe

The publisher and the cleanup hold a token that can push packages. Keep them
independent of the build: do not add a checkout of PR code, PR scripts, shared
build caches or running the image to them. Download artifacts only from the
build run the publisher approved, and derive destinations and PR numbers from
GitHub's API, never from artifact contents. Pin the actions they use by
commit.
