# Pull request images

PRs targeting master, including forks, build without registry credentials in
`docker-pr-build.yml`. It is the only workflow that builds a PR's images:
`docker-build.yml` runs on pushes to master and on tags alone. The build runs
`scripts/build-images.sh`, the same script a release uses, for every
architecture a release has, and its `--oci-dir` output -- one OCI directory
layout holding all of them -- is retained as an Actions artifact for three
days.

After a successful build, `docker-pr-publish.yml` runs from the default branch
on a fresh GitHub-hosted runner. It validates the source workflow and finds an
open PR whose head still matches the built commit. It copies the layout with
Skopeo; it never executes the image or checks out PR code.

Each commit is published as one multi-architecture image (amd64, arm64 and
armv7), so `docker pull` picks the host's:

- `ghcr.io/<owner>/<repo>-preview:pr-<number>-<full-head-sha>`

After the push succeeds, the publisher posts a PR comment with the image
reference and pull command. Re-running the same commit updates its existing
comment. The publishing job also lists the image in the run summary. Preview images
contain untrusted PR code. Their tags and package are separate from releases;
there is no mutable `latest` or `pr-<number>` alias. The publisher intentionally
rejects closed PRs and runs superseded by a new commit.

The publishing workflow must land on the default branch before it can run.
Fork builds remain subject to GitHub's contributor approval policy. The preview
GHCR package needs public visibility for anonymous pulls (new packages may
initially be private), and the repository must retain Actions write access to
that package. No personal access token or deploy key is required.

Keep the publisher independent of the build workspace: do not add PR checkout,
PR scripts, shared build caches, or image execution to it. Download artifacts
only from the triggering run, and derive destinations from GitHub API metadata,
never from artifact contents.
