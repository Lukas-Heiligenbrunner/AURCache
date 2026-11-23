# Design: Pull request images

Status: **Implemented** · Last updated: 2026-11-23

Every pull request's server image, forks included, is built by
`docker-pr-build.yml` with a read-only token. When the build of a PR labelled
`preview-image` succeeds, `docker-pr-publish.yml` pushes it to
`ghcr.io/<owner>/<repo>-preview:pr-<number>-<sha>`, and moves `pr-<number>`
to it. The publishing run's summary names the image.

## Setup

1. Merge both workflows to the default branch: the publisher only runs from
   there.
2. Create the `preview-image` label.
3. After the first publish creates the preview package, make it public for
   anonymous pulls, and keep the repository's write access to it (package
   settings, Manage Actions access). No personal access token or deploy key is
   needed.

Only a build that finishes while the label is on is published: after
labelling a PR, re-run its latest build.

## What the label trusts

Labelling a PR trusts its head branch, not the commit that was reviewed.
While the label is on, every successful build of the PR's current head is
published, force-pushes included, so anyone who can push to that branch gets
their commits published. Removing the label stops later publishes, not one
already under way. Do not let automation apply this label.

A preview image is untrusted PR code. Only its `pr-<number>-<sha>` tag comes
from the publisher; everything in the image, annotations included, comes from
the PR's build.

The publish job never runs PR code, and its token can push to every package
this repository's workflows can write, the release images included. The
artifact is the only thing it takes from the build, and only the download
action's unzip, `find` and skopeo read it; a flaw in one of those is what
would hand a PR that token. Keep it that way: no checkout of PR code, no PR
scripts, no shared build caches, never run the image, take nothing from the
artifact that picks a destination, and pin the publisher's actions by commit.

skopeo fetches a blob from the URLs that its descriptor lists (`urls`) before
it reads the copy in the layout, so a PR could make the publisher send a GET to
an address that the PR picks. The push step sets `HTTP_PROXY` and `HTTPS_PROXY`
to a closed port, and `NO_PROXY` to the hosts that a push and the retag reach:
`ghcr.io` (the API, tokens and uploads, whose `Location` is relative) and
`pkg-containers.githubusercontent.com` (where `ghcr.io` redirects a blob
download). Any other host fails to connect. This blocks the request in the
network instead of parsing the layout, so a JSON quirk that one parser reads
differently from skopeo's cannot reopen it. Go never proxies loopback, so a URL
on the runner's own loopback is still fetched; and a URL on a `NO_PROXY` host
is fetched too, with no credentials. If GHCR starts redirecting to another
host, publishing fails until `NO_PROXY` lists it.
