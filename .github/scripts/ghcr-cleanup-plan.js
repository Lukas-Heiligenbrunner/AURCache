// Which pull-request preview tags ghcr-cleanup.yml may delete, per package.
//
// A preview tag names its pull request: `pr-<n>-<commit>` in the preview
// package (docker-pr-publish.yml), or `pr-<n>` from the builds that used to
// publish into the release packages. Either may end in `-aarch64`, the
// per-architecture tag those used to carry. Each pull request and suffix is
// one series. An open pull request keeps the newest tag of each of its
// series; every other preview tag is listed. The cleanup action then deletes
// only those older than its `older-than`, so a listed tag still lives a week.
//
// Nothing else is listed: release and moving tags (`v*`, `latest`, `git`) are
// never deleted, and the images they no longer point to are untagged, which
// the action handles on its own.
//
// Called from actions/github-script; returns the matrix ghcr-cleanup.yml runs.

const PREVIEW_TAG = /^pr-(\d+)(?:-[0-9a-f]{40})?(-aarch64)?$/;

module.exports = async ({ github, context, core }, packages) => {
  const { owner } = context.repo;
  const { data: account } = await github.rest.users.getByUsername({
    username: owner,
  });
  const listVersions =
    account.type === 'Organization'
      ? (name) =>
          github.paginate(
            github.rest.packages.getAllPackageVersionsForPackageOwnedByOrg,
            { package_type: 'container', package_name: name, org: owner, per_page: 100 },
          )
      : (name) =>
          github.paginate(
            github.rest.packages.getAllPackageVersionsForPackageOwnedByUser,
            { package_type: 'container', package_name: name, username: owner, per_page: 100 },
          );

  // Only a definite answer counts as closed: a pull request that does not
  // exist has nothing to keep, but any other failure stops the run rather
  // than reading as "closed" and deleting an open pull request's image.
  const open = new Map();
  const isOpen = async (number) => {
    if (!open.has(number)) {
      try {
        const { data } = await github.rest.pulls.get({
          owner,
          repo: context.repo.repo,
          pull_number: number,
        });
        open.set(number, data.state === 'open');
      } catch (error) {
        if (error.status !== 404) throw error;
        open.set(number, false);
      }
    }
    return open.get(number);
  };

  const matrix = [];
  const summary = [];
  for (const name of packages) {
    let versions;
    try {
      versions = await listVersions(name);
    } catch (error) {
      // The preview package appears with the first preview image.
      if (error.status !== 404) throw error;
      core.info(`${name}: no such package yet, skipped`);
      continue;
    }

    const series = new Map();
    for (const version of versions) {
      for (const tag of version.metadata.container.tags) {
        const match = PREVIEW_TAG.exec(tag);
        if (!match) continue;
        const key = `${match[1]}${match[2] ?? ''}`;
        if (!series.has(key)) series.set(key, { pr: Number(match[1]), tags: [] });
        series.get(key).tags.push({ tag, created: version.created_at });
      }
    }

    const deleted = [];
    const kept = [];
    for (const { pr, tags } of series.values()) {
      tags.sort((a, b) => b.created.localeCompare(a.created));
      const keep = (await isOpen(pr)) ? 1 : 0;
      kept.push(...tags.slice(0, keep).map((t) => t.tag));
      deleted.push(...tags.slice(keep).map((t) => t.tag));
    }

    matrix.push({ package: name, 'delete-tags': deleted.join(',') });
    summary.push([name, String(versions.length), String(deleted.length), kept.join(' ') || '—']);
    core.info(`${name}: ${versions.length} versions; preview tags listed: ${deleted.length}; kept: ${kept.join(' ') || 'none'}`);
  }

  await core.summary
    .addHeading('Image cleanup plan', 3)
    .addTable([
      [
        { data: 'Package', header: true },
        { data: 'Versions', header: true },
        { data: 'Preview tags listed', header: true },
        { data: 'Kept: open pull requests’ latest', header: true },
      ],
      ...summary,
    ])
    .addRaw('Listed tags are deleted once older than a week, along with untagged images of that age.')
    .write();

  return matrix;
};
