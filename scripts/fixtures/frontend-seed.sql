-- Fixture data for scripts/test-frontend.sh.
--
-- Applied after the server has started, so the schema already exists. That
-- ordering also keeps the run deterministic: the version-check scheduler runs
-- one pass at boot before it sleeps, and at that point this table is empty, so
-- it has nothing to rewrite. The script sets a long VERSION_CHECK_INTERVAL so
-- the next pass never lands inside the run.
--
-- The package list deliberately covers every status badge and the awkward
-- names: `2048.c` and `python-3.11` look like filenames, `aewm++` contains the
-- character that is literal in a path but a space in a query value.

DELETE FROM builds;
DELETE FROM packages;

INSERT INTO packages (name, status, out_of_date, upstream_version, build_flags, platforms, source_data, directly_requested) VALUES
  ('hello',                  2, 0, '2.12.1-2',  '', 'x86_64', '{"type":"aur","name":"hello"}',                  1),
  ('neofetch',               1, 1, '7.1.0-2',   '--noconfirm;--nocolor', 'x86_64', '{"type":"aur","name":"neofetch"}', 1),
  ('yay',                    0, 0, '12.4.2-1',  '', 'x86_64', '{"type":"aur","name":"yay"}',                    1),
  ('paru',                   2, 0, '2.0.4-1',   '', 'x86_64', '{"type":"aur","name":"paru"}',                   1),
  ('visual-studio-code-bin', 3, 0, '1.92.0-1',  '', 'x86_64', '{"type":"aur","name":"visual-studio-code-bin"}', 1),
  ('2048.c',                 4, 0, '1.0-3',     '', 'x86_64', '{"type":"aur","name":"2048.c"}',                 1),
  ('aewm++',                 1, 0, '1.1.6-4',   '', 'x86_64', '{"type":"aur","name":"aewm++"}',                 1),
  ('python-3.11',            1, 0, '3.11.9-1',  '', 'x86_64', '{"type":"aur","name":"python-3.11"}',            1),
  -- Deliberately gets no build below, and no upstream version either: this is
  -- a package that was just promoted from a dependency and has not been
  -- version-checked yet. Both "no version yet" placeholders end up on screen
  -- rather than only in a unit test.
  ('never-built',            3, 0, NULL,        '', 'x86_64', '{"type":"aur","name":"never-built"}',            1),
  -- Dependency-only packages: nobody asked for these, something else needs
  -- them. They are what the dashboard's second package count counts, and what
  -- the package list keeps behind its "Dependencies" checkbox.
  ('libfoo',                 1, 0, '2.3.1-1',   '', 'x86_64', '{"type":"aur","name":"libfoo"}',                 0),
  ('libbar',                 1, 0, '0.9-2',     '', 'x86_64', '{"type":"aur","name":"libbar"}',                 0),
  -- A git-sourced package. It has no AUR entry at all, so its description,
  -- licenses and maintainer can only come from its checkout — and its origin
  -- link has to point at the repository rather than at the AUR.
  ('my-tool-git',            1, 0, 'r42.abc1234-1', '', 'x86_64',
   '{"type":"git","url":"https://github.com/example/my-tool","ref":"main","subfolder":""}', 1);

-- Timestamps are Unix *seconds*: `aurcache-api/src/stats.rs` compares
-- start_time against strftime('%s', 'now'). Milliseconds render as dates
-- decades in the future.
--
-- The spread exercises every branch of the duration and age formatters,
-- including a build that started but never finished (NULL end_time), which
-- must read as unknown rather than as a zero-length build.
-- No log: build logs are files on disk now, not a column, so a fixture cannot
-- seed one. The build pages render "no log for this build", which is the
-- correct answer for a build whose log is not there.
INSERT INTO builds (pkg_id, number, status, start_time, end_time, platform, version)
SELECT
  p.id,
  1,
  p.status,
  CAST(strftime('%s','now') AS INTEGER) - offs.age,
  CASE WHEN offs.dur IS NULL THEN NULL
       ELSE CAST(strftime('%s','now') AS INTEGER) - offs.age + offs.dur END,
  'x86_64',
  p.upstream_version
FROM packages p
JOIN (
  SELECT 'hello' AS name,                  120    AS age, 43   AS dur UNION ALL
  SELECT 'neofetch',                       900,           187         UNION ALL
  SELECT 'yay',                            18000,         3720        UNION ALL
  SELECT 'paru',                           172800,        61          UNION ALL
  SELECT 'visual-studio-code-bin',         300,           NULL        UNION ALL
  SELECT '2048.c',                         777600,        7860        UNION ALL
  SELECT 'aewm++',                         45,            12          UNION ALL
  SELECT 'python-3.11',                    93600,         900
) offs ON offs.name = p.name;

-- AUR metadata as the version-check scheduler mirrors it. Seeding it keeps the
-- fixture offline: without it the package route falls back to a live AUR
-- lookup for every package.
UPDATE packages SET
  source_description      = 'Prints Hello World and more',
  source_maintainer       = 'someone',
  source_project_url      = 'https://www.gnu.org/software/hello/',
  source_licenses         = 'GPL-3.0-or-later',
  source_first_submitted  = 1425168000,
  source_last_modified    = 1755000000,
  aur_flagged_outdated = 0,
  aur_missing = 0
WHERE name = 'hello';

UPDATE packages SET
  source_description      = 'Yet another yogurt. Pacman wrapper and AUR helper written in go.',
  source_maintainer       = 'jguer',
  source_project_url      = 'https://github.com/Jguer/yay',
  source_licenses         = 'GPL-3.0-or-later',
  source_first_submitted  = 1470000000,
  source_last_modified    = 1756000000,
  aur_flagged_outdated = 0,
  aur_missing = 0
WHERE name = 'yay';

UPDATE packages SET
  source_description     = 'A tool built straight from git',
  source_maintainer      = 'Alex <alex@example.com>',
  source_project_url     = 'https://example.com/my-tool',
  source_licenses        = 'MIT',
  source_first_submitted = 1600000000,
  source_last_modified   = 1756000000
WHERE name = 'my-tool-git';

-- The builds above take their status from the package row, which is right for
-- everything except `hello`: its package status reflects the failed build added
-- below, while the build that produced what is in the repository succeeded.
UPDATE builds SET status = 1, version = '2.12.1-1'
WHERE pkg_id = (SELECT id FROM packages WHERE name = 'hello');

-- A second, newer build of `hello` that failed. The package page shows both
-- "Latest" and "in repo" only when they differ, and that gap -- newest attempt
-- broken, repository still serving something older -- is the case worth having
-- on screen.
INSERT INTO builds (pkg_id, number, status, start_time, end_time, platform, version)
SELECT p.id,
       2,
       2,
       CAST(strftime('%s','now') AS INTEGER) - 60,
       CAST(strftime('%s','now') AS INTEGER) - 4,
       'x86_64',
       '2.12.1-2'
FROM packages p WHERE p.name = 'hello';

-- A dependency graph with all three interesting states, so the package page's
-- "blocking" markers are on screen rather than only in a unit test:
--   yay -> hello        satisfied (built, no constraint)
--   yay -> python-3.11  built successfully, but to a version the constraint
--                       rejects -- the case a status badge alone cannot show,
--                       since the dependency looks healthy everywhere else
--   yay -> never-built  no successful build at all
DELETE FROM dependencies;
INSERT INTO dependencies (dependent_id, dependee_id, version_constraint)
SELECT d.id, e.id, v.constraint_text
FROM (
  SELECT 'yay' AS dependent, 'hello'       AS dependee, ''       AS constraint_text UNION ALL
  SELECT 'yay',              'python-3.11',             '>=99.0'                    UNION ALL
  SELECT 'yay',              'never-built',             '>=1.0'
) v
JOIN packages d ON d.name = v.dependent
JOIN packages e ON e.name = v.dependee;

-- A setting stored globally, so the settings page has one row in each of the
-- three states it renders differently: env-locked (VERSION_CHECK_INTERVAL is
-- set for the fixture server), stored, and never set. Stored is the state a
-- user is in the moment after they save one, and the only one that offers a
-- Reset. `-1` is the global scope; a real package id would make it per-package.
--
-- Both auto-rebuild paths are on by default, which makes every out-of-date
-- package "rebuilding on its own" and collapses the dashboard's Out of date
-- card, which lists the ones that need a hand. So both are stored off: the
-- schedule as `@never`, and building on a new version disabled.
INSERT INTO settings (key, value, pkg_id) VALUES
  ('job_timeout', '7200', -1),
  ('auto_update_schedule', '@never', -1),
  ('build_on_new_version', 'false', -1);

-- A few lines of the log. Each is what the server would have written: the
-- kind, the severity it implies (0 info, 1 warning, 2 error), the sentence it
-- renders, and the payload the browser renders it from -- so the payloads have
-- to be shapes the catalogue parses (aurcache-common/src/api/events.rs). The
-- index rows below are what the entity filter reads.
--
-- Explicit ids, well clear of the server's own start entry, which it writes
-- before this runs.
--
-- One row has no user, which is the case the screen renders differently: nobody
-- asked for it, a schedule did.
--
-- Arranged around the server-start row at -1000s, because that is the marker
-- "since the last restart" counts back to: two entries are older than it and
-- must drop out when that filter is on. The two failures give the severity
-- filter something to find, one of each level.
INSERT INTO log (id, kind, severity, message, data, timestamp, user) VALUES
  (1001, 'package.added', 0, 'added package hello', '{"pkg":"pkg:hello"}',
    CAST(strftime('%s','now') AS INTEGER) - 30, 'alice'),
  (1002, 'publish.failed', 2, 'publishing yay #7 failed: no space left on device',
    '{"build":"build:yay/7","error":"no space left on device"}',
    CAST(strftime('%s','now') AS INTEGER) - 100, NULL),
  (1003, 'worker.reaped', 1, 'a worker stopped answering: requeued hello #12, gave up on hello #11',
    '{"retried":["build:hello/12"],"failed":["build:hello/11"]}',
    CAST(strftime('%s','now') AS INTEGER) - 200, NULL),
  -- A worker entry, so the log has a reference that opens a worker rather
  -- than a package.
  (1004, 'worker.approved', 0, 'approved worker builder-01', '{"worker":"worker:builder-01"}',
    CAST(strftime('%s','now') AS INTEGER) - 300, 'alice'),
  (1005, 'build.queued', 0, 'queued a build of yay (rebuild)', '{"pkg":"pkg:yay","cause":"rebuild"}',
    CAST(strftime('%s','now') AS INTEGER) - 900, 'alice'),
  (1006, 'server.start', 0, 'AURCache 0.5.0 started', '{"version":"0.5.0"}',
    CAST(strftime('%s','now') AS INTEGER) - 1000, NULL),
  (1007, 'package.deleted', 0, 'deleted package obsolete-thing', '{"pkg":"pkg:obsolete-thing"}',
    CAST(strftime('%s','now') AS INTEGER) - 4000, 'bob'),
  (1008, 'build.queued', 0, 'queued a build of neofetch (new version 7.1.0-2)', '{"pkg":"pkg:neofetch","cause":"update","version":"7.1.0-2"}',
    CAST(strftime('%s','now') AS INTEGER) - 86000, NULL);

INSERT INTO log_entity (log_id, role, ns, id) VALUES
  (1001, 'pkg', 'pkg', 'hello'),
  (1002, 'build', 'pkg', 'yay'),
  (1002, 'build', 'build', 'yay/7'),
  (1003, 'retried', 'pkg', 'hello'),
  (1003, 'retried', 'build', 'hello/12'),
  (1003, 'failed', 'pkg', 'hello'),
  (1003, 'failed', 'build', 'hello/11'),
  (1004, 'worker', 'worker', 'builder-01'),
  (1005, 'pkg', 'pkg', 'yay'),
  (1007, 'pkg', 'pkg', 'obsolete-thing'),
  (1008, 'pkg', 'pkg', 'neofetch');

-- One config file stored, one left unset, so the page shows both states it
-- renders differently: "stored" with a Reset, and "builder default" without.
-- `-1` is the global scope.
INSERT INTO settings (key, value, pkg_id) VALUES
  ('makepkg_conf', '# Seeded makepkg.conf' || char(10) || 'MAKEFLAGS="-j8"' || char(10) || 'PACKAGER="AURCache <build@example.invalid>"', -1);

-- A per-package override, so the package settings page has both of its states
-- to render: `neofetch` holds its own pacman.conf while `hello` inherits the
-- server-wide one. `pkg_id` is looked up rather than written as a literal
-- because it comes from an autoincrement above.
-- On `makepkg_conf` rather than `pacman_conf` because it is the tab that opens
-- by default, and a DOM dump only carries the tab that is showing.
INSERT INTO settings (key, value, pkg_id) VALUES
  ('makepkg_conf', '# neofetch-only makepkg.conf', (SELECT id FROM packages WHERE name = 'neofetch'));

-- Builds spread back over the year, purely so the dashboard graph has a curve.
-- It groups by month over the last twelve, and every build seeded above landed
-- in the last few days — one point is a dot, not a line.
--
-- Numbered from 10 up so they cannot collide with the builds above under
-- `UNIQUE (pkg_id, number)`, and given end times so they count as finished.
INSERT INTO builds (pkg_id, number, status, start_time, end_time, platform, version)
SELECT
  p.id,
  10 + offs.n,
  1,
  CAST(strftime('%s','now',  '-' || offs.months || ' months') AS INTEGER),
  CAST(strftime('%s','now',  '-' || offs.months || ' months') AS INTEGER) + 120,
  'x86_64',
  '1.0-1'
FROM packages p
JOIN (
  SELECT 1 AS n, 1 AS months UNION ALL
  SELECT 2, 2  UNION ALL
  SELECT 3, 3  UNION ALL
  SELECT 4, 3  UNION ALL
  SELECT 5, 4  UNION ALL
  SELECT 6, 6  UNION ALL
  SELECT 7, 6  UNION ALL
  SELECT 8, 6  UNION ALL
  SELECT 9, 8
) offs
WHERE p.name = 'hello';

-- More builds than fit on a page, so pagination is exercised against a real
-- list rather than only in a unit test. `paru` because nothing else in the
-- route list or the interaction tests looks at it, and dated two months back
-- so the dashboard's "this week" figures are unaffected. Numbered from 100 up
-- to stay clear of the builds above under `UNIQUE (pkg_id, number)`.
WITH RECURSIVE seq(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 110
)
INSERT INTO builds (pkg_id, number, status, start_time, end_time, platform, version)
SELECT p.id,
       100 + seq.n,
       1,
       CAST(strftime('%s','now','-2 months') AS INTEGER) + seq.n * 60,
       CAST(strftime('%s','now','-2 months') AS INTEGER) + seq.n * 60 + 90,
       'x86_64',
       '2.0.4-1'
FROM packages p, seq
WHERE p.name = 'paru';

-- A fleet with one worker in each state, because each renders differently and
-- pending is the one the page exists to surface.
--
-- `signed_cert` is left NULL throughout: the list endpoint no longer returns
-- it, and a fixture carrying a fake PEM would only suggest it mattered here.
DELETE FROM workers;
-- Ids are explicit because the worker page is reached at `/worker/<id>`, and a
-- route check naming one has to land on the same machine every run.
INSERT INTO workers
  (id, name, status, cert_fingerprint, native_arches, emulated_arches, last_seen, version, package_affinity, priority, settings_declaration)
VALUES
  -- Approved, busy, and tuned: reserved for one package and preferred over the
  -- others, so both of those columns have something to show.
  (1, 'builder-01', 'approved', '1111111111111111aaaa1111111111111111aaaa1111111111111111aaaa1111', 'x86_64', '',
   CAST(strftime('%s','now') AS INTEGER) - 5, '0.1.0', 'visual-studio-code-bin', 10, '[]'),
  -- Approved, but only reaches aarch64 through emulation. Its reservation
  -- names no known package, so the column has to leave it as plain text.
  (2, 'builder-arm', 'approved', '2222222222222222bbbb2222222222222222bbbb2222222222222222bbbb2222', 'aarch64', 'armv7h',
   CAST(strftime('%s','now') AS INTEGER) - 200000, '0.1.0', 'not-a-package', 0, '[]'),
  -- Enrolled and waiting. Has never checked in, so "last seen" is never rather
  -- than a long time ago.
  (3, 'new-machine', 'pending', '3333333333333333cccc3333333333333333cccc3333333333333333cccc3333', 'x86_64', '',
   NULL, '0.1.0', '', 0, '[]'),
  -- Retired. Kept so old builds still name the machine that ran them, and
  -- hidden behind the toggle by default.
  (4, 'old-builder', 'revoked', '4444444444444444dddd4444444444444444dddd4444444444444444dddd4444', 'x86_64', '',
   CAST(strftime('%s','now') AS INTEGER) - 5000000, '0.0.9', '', 0, '[]'),
  -- A machine replaced by another of the same hostname: the retired row keeps
  -- the name, so two workers answer to it. This is why worker names are not
  -- unique and the URL has a way past them.
  (5, 'replaced-host', 'approved', '5555555555555555eeee5555555555555555eeee5555555555555555eeee5555', 'x86_64', '',
   CAST(strftime('%s','now') AS INTEGER) - 20, '0.1.0', '', 0, '[]'),
  (6, 'replaced-host', 'revoked', '6666666666666666ffff6666666666666666ffff6666666666666666ffff6666', 'x86_64', '',
   CAST(strftime('%s','now') AS INTEGER) - 9000000, '0.0.9', '', 0, '[]');

-- What builder-01 declares it can be configured with, and what those settings
-- actually resolved to on it. Stored exactly as a worker reports them: an array
-- of declarations, and a map of effective values keyed by the same names.
--
-- One of them is rejected on purpose. `WORKER_BUILDDIR_MAX_BYTES=450 giraffes`
-- is the shape of the failure this page exists for -- a value that was
-- configured, did not parse, and left the machine silently running the default.
UPDATE workers SET
  settings_declaration = '[
    {"key":"concurrency","kind":{"type":"integer","min":1},
     "description":"How many builds this worker runs at once.",
     "category":"Scheduling","default":"1","env_var":"WORKER_CONCURRENCY",
     "applies":"next_job"},
    {"key":"builddir_max_bytes","kind":{"type":"size"},
     "description":"Total disk the kept build trees may occupy.",
     "category":"Build trees","default":"200G",
     "env_var":"WORKER_BUILDDIR_MAX_BYTES","applies":"next_loop"},
    {"key":"build_memory_max","kind":{"type":"size"},
     "description":"Memory one build may use before it is killed.",
     "category":"Build limits","env_var":"WORKER_BUILD_MEMORY_MAX",
     "applies":"next_job"}
  ]',
  effective_config = '{
    "settings":{
      "concurrency":{"value":"2","source":"env","status":"applied"},
      "builddir_max_bytes":{"value":"200G","source":"default","status":"rejected",
        "reason":"WORKER_BUILDDIR_MAX_BYTES ignored: \"450 giraffes\" is not a size (bytes, or a size such as 500M, 450G, 450GiB or 450GB)"},
      "build_memory_max":{"source":"default","status":"applied"}
    }
  }'
 WHERE name IN ('builder-01', 'replaced-host') AND status = 'approved';

-- Attribute builds to the machines that ran them, so the fleet page has a
-- record to report rather than "no builds yet" on every row. The split is
-- deliberate: builder-01 takes the bulk and nearly always succeeds, builder-arm
-- takes a handful and fails some of them, which is the comparison the page
-- exists to make. `yay`'s build is the active one, so builder-01 also has
-- something in flight.
UPDATE builds SET worker_id = (SELECT id FROM workers WHERE name = 'builder-01')
 WHERE pkg_id IN (SELECT id FROM packages WHERE name IN ('paru', 'yay'));

-- The one build that is still running has to look like it: a live lease and a
-- recent start. The reaper requeues an ACTIVE build whose lease has lapsed *or*
-- whose start is older than the job timeout -- either alone took this build
-- away twenty seconds into the run, and the page lost the state it was there
-- to show.
UPDATE builds
   SET lease_expires_at = CAST(strftime('%s','now') AS INTEGER) + 3600,
       start_time = CAST(strftime('%s','now') AS INTEGER) - 60,
       end_time = NULL
 WHERE pkg_id = (SELECT id FROM packages WHERE name = 'yay');
UPDATE builds SET worker_id = (SELECT id FROM workers WHERE name = 'builder-arm')
 WHERE pkg_id = (SELECT id FROM packages WHERE name = 'hello');

-- Built artifacts for the package page's file list. `hello` is a split package
-- here so the list has more than one row, and one row is deliberately left
-- without a size: that is what a row written before the size column looks like
-- until the startup backfill reaches it, and the page must render it as unknown
-- rather than as a zero-byte package.
INSERT INTO files (filename, platform, package_id, size) VALUES
  ('hello-2.12.1-2-x86_64.pkg.tar.zst',      'x86_64',
   (SELECT id FROM packages WHERE name = 'hello'), 1258291),
  ('hello-docs-2.12.1-2-x86_64.pkg.tar.zst', 'x86_64',
   (SELECT id FROM packages WHERE name = 'hello'), 40960),
  ('neofetch-7.1.0-2-x86_64.pkg.tar.zst',    'x86_64',
   (SELECT id FROM packages WHERE name = 'neofetch'), NULL),
  -- `aewm++` is the one package here whose latest build succeeded, so it is
  -- where the build column and the package column can be seen agreeing: both
  -- report 512 KiB.
  ('aewm++-1.1.6-4-x86_64.pkg.tar.zst',      'x86_64',
   (SELECT id FROM packages WHERE name = 'aewm++'), 500000),
  ('aewm++-docs-1.1.6-4-x86_64.pkg.tar.zst', 'x86_64',
   (SELECT id FROM packages WHERE name = 'aewm++'), 24576);

-- Size the builds the way the startup backfill does: the newest *successful*
-- build of each package and platform owns the artifacts currently on disk, so
-- it gets their total, and every other build keeps NULL. That is what makes a
-- failed build's size column a dash rather than a stale number.
UPDATE builds
   SET size = (SELECT CASE WHEN COUNT(*) = COUNT(f.size) THEN SUM(f.size) END
                 FROM files f
                WHERE f.package_id = builds.pkg_id
                  AND f.platform = builds.platform)
 WHERE status = 1
   AND number = (SELECT MAX(b2.number)
                   FROM builds b2
                  WHERE b2.pkg_id = builds.pkg_id
                    AND b2.platform = builds.platform
                    AND b2.status = 1);

-- Peak memory for the `hello` builds, so the Builds list renders a real figure
-- in that column rather than only the dash every other row shows. The two
-- states matter: a build that reported one, and the builds that did not.
UPDATE builds SET peak_memory = 6871947673
WHERE pkg_id = (SELECT id FROM packages WHERE name = 'hello');

-- Disk usage for `hello`'s first build, part by part, so the build page renders
-- its figures. No build tree: a part not measured is left out, not shown as 0.
UPDATE builds SET disk_chroot = 3221225472, disk_workdir = 41943040, disk_sources = 1048576
WHERE number = 1 AND pkg_id = (SELECT id FROM packages WHERE name = 'hello');
