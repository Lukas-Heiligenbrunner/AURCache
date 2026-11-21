# Pull request images

Every pull request's server image, forks included, is built by
`docker-pr-build.yml` with a read-only token. For PRs labelled
`preview-image`, `docker-pr-publish.yml` pushes it to
`ghcr.io/<owner>/<repo>-preview` and links it from the PR. The workflows'
comments say how each step works.

## Setup

1. Merge both workflows to the default branch: the publisher only runs from
   there.
2. Create the `preview-image` label.
3. After the first publish creates the preview package, make it public for
   anonymous pulls, and keep the repository's write access to it (package
   settings, Manage Actions access). No personal access token or deploy key is
   needed.

## What the label trusts

Labelling a PR is a trust decision about its head branch, not about the commit
that was reviewed. While the label is on, every successful build of the PR's
current head is published, new pushes and force-pushes included. Anyone who
can push to that branch gets their commits published: the PR's author,
whoever the fork's owner lets push to it, or anyone with write access for a
branch of this repository. Removing the label stops publishing. Only people
with triage access can label a PR; do not let automation apply this label.

A preview image is untrusted PR code. The publisher sets the annotations on
its index (revision, source, url, description) from GitHub's API. Those
annotations and the `pr-<number>-<sha>` tag say which commit the image holds.
Everything else in the image comes from the PR's build.

The publish job never runs PR code. Its token can push to every package this
repository's workflows can write, the release images included, and it can
read the repository's Actions runs and pull requests. The artifact is the
only thing it takes from the build. Only node reads that artifact (the
download action's unzip, over node's zlib, and the layout check), and then
Go (skopeo). A flaw in one of those is what would hand a PR that token. Keep
it that way: no checkout of PR code, no PR scripts, no shared build caches,
never run the image, take nothing from the artifact that picks a
destination, and pin the publisher's actions by commit.
