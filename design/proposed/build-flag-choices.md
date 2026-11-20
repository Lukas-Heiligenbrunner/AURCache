# Build flags as a set of choices

Status: **Proposed** · Last updated: 2026-11-20

A package's build flags are typed into a text field, one at a time, and the
server refuses whatever is not on its list of makepkg options. The list is
fixed now, so a text field only gives people a chance to type something that
will be refused. Most of the list does nothing on a worker anyway. This
replaces the text field with a dialog: each option a package can choose is
shown with a line saying what it does, and the package page shows only the
flags that are set. The flags every build needs stop being stored per package,
because the worker passes them itself.

---

## Where things stand

- **The list.** `aurcache-common/src/build_flags.rs` names every makepkg 7.1
  option as allowed or refused (with a reason). `canonical` turns flags into
  long names, splits bundles such as `-cf` and drops duplicates. The API's
  `parse_build_flags` (`aurcache-api/src/package.rs:117`) and the dump restore
  (`aurcache-utils/src/restore.rs`) go through it.
- **The default.** A new package gets `--noconfirm --noprogressbar --nocolor`
  (`aurcache-utils/src/package/add.rs:54`), and the 0.5.0 migration upgrades
  every package to the same three (`upgrade_from_paru`). So almost every
  stored list holds the same three flags.
- **On the wire.** The job descriptor carries the stored list as it is
  (`job_descriptor.rs:67`). The chroot worker passes it after `--` to
  `makechrootpkg`, which already adds `--syncdeps --noconfirm --log --holdver
  --skipinteg` inside the chroot and forwards `--skip*`, `--holdver` and
  `--ignorearch` to its `--verifysource` pass. The docker worker runs
  `makepkg --config … -s <flags>` and adds nothing.
- **The UI.** `BuildFlagsField` (`frontend-rs/src/screens/package/config.rs`)
  shows the flags as removable chips, with a text box and *Add*.

## What each allowed option does on a worker

| Option | On a worker |
|---|---|
| `--nocheck`, `--noprepare`, `--holdver`, `--ignorearch`, `--nodeps`, `--skipchecksums`, `--skippgpcheck` | A real choice for a package |
| `--skipinteg` | Exactly `--skipchecksums` plus `--skippgpcheck` |
| `--noconfirm`, `--noprogressbar`, `--nocolor` | Every build needs them; nobody should turn them off |
| `--syncdeps` | Both workers already pass it |
| `--check` | `check()` already runs unless `--nocheck` is set |
| `--clean`, `--cleanbuild` | Nothing in a throwaway chroot; with a persistent build directory, they undo the setting the package chose |
| `--force`, `--rmdeps` | Nothing: the chroot is new and is thrown away afterwards |
| `--needed`, `--asdeps` | Only apply to `--install`, which is refused |
| `--nosign` | Nothing: workers hold no signing key |

## Design

### Options come in four kinds

`build_flags.rs` gives each option one of four uses, replacing
`Allowed`/`Refused`:

- **Offered**, with a one-line description: the seven real choices. These are
  the only flags a package stores.
- **Expands** into other options: `--skipinteg` becomes `--skipchecksums
  --skippgpcheck`. Its letter has no short form to deal with.
- **Dropped**: accepted, and removed from what is stored. These are the
  options in rows 3 to 8 of the table above.
- **Refused**, with its reason, as now. The refused set does not grow.

Nothing 0.6.0 accepted becomes refused; it is either offered or dropped.
`load_dump` rejects a whole dump that names one refused flag, so refusing
`--clean` now would make a 0.6.0 dump that used it impossible to restore. A
CLI script that passes `--noconfirm` keeps working too, and stores the list it
would have stored without it.

`--clean` and `--cleanbuild` are not offered. Without a persistent build
directory they do nothing. With one, `--cleanbuild` deletes `$srcdir` before
every build and `--clean` deletes it after every successful one, so the kept
tree is never reused and the setting does nothing. Turning the setting off says
that plainly. A tree left in a bad state needs no flag either: a failed build
already throws its tree away, so the retry starts cold
(`design/implemented/persistent-build-directory.md`).

`canonical` keeps its signature and returns only offered flags, in the order
of the offered list rather than the order given. Then two lists with the same
choices are equal, and the UI can show them in a fixed order.

The offered list is public (`pub const OFFERED: &[Offered]`, each a `flag` and
a `summary`), so the dialog is generated from the same table the server checks
against. The frontend already depends on `aurcache-common` and is built with
the server, so the two cannot drift.

### The worker passes the three every build needs

`aurcache-worker-core` exports the three (`--noconfirm --noprogressbar
--nocolor`). Both executors put them before the job's flags: the chroot worker
after `--` in `build_command`, and the docker worker after `-s` in
`build_build_command`. A new package stores an empty list, which means "build
as makepkg would by default".

**Wire compatibility.** Worker protocol 1 says `build_flags` are "arguments
added to makepkg's command line, as given", and that does not change. But a
0.6.0 docker worker adds nothing of its own, so if the server stopped sending
`--noconfirm` it would build without it. So the server keeps adding the three
to the job descriptor (`job_descriptor.rs`) for workers on protocol 1. A new
worker given them twice drops the copies, because makepkg treats a repeated
flag the same as one. When protocol 2 exists, it can say workers add the
three themselves, and the server can stop sending them.

### Startup reduces stored lists to offered flags, and says what it dropped

This is not a migration, because migrations run in `init_db`, before the
activity log exists. It is a startup step, `startup::settle_build_flags`,
running after `activitylog::spawn` as `report_untrusted_workers` does, with
its logic in `aurcache-utils`. It runs every package's stored list through
`canonical` and writes back the lists that changed, one transaction per
package. Running it again changes nothing, so it can run on every start; if
the offered set changes in a later release, the same step tidies up after it.
A list `canonical` refuses (impossible, since 0.6.0 refused at input) keeps
its offered flags.

For each package that lost a flag other than `--noconfirm`,
`--noprogressbar` or `--nocolor` (which the worker now passes, so nothing
changes), it emits an **Info** event, `BuildFlagsDropped { pkgbase, dropped
}`, in the event catalogue in `aurcache-common`. The event says the flags had
no effect on a build and are no longer stored. The case it is for is a
package with `--cleanbuild` and a persistent build directory, which now
starts reusing its tree.

`upgrade_from_paru` then returns `[]` for both 0.5.0 defaults. Its test
changes to say so, and its doc comment drops "the current default". The 0.5.0
migration keeps its `tracing::warn!` for the flags no build can take; those
are refused, not dropped, and the activity log does not exist yet when it
runs.

### The UI: a summary and a dialog

On the package page, the *Flags* row shows the set flags as read-only chips in
`font-mono`, or "makepkg defaults" when there are none, with an *Edit* button.

*Edit* opens a `Modal` (`widgets.rs`). It has one row per offered option,
styled like the add-package dialog's search results (`results_list` in
`package_add.rs`): a checkbox, then the flag in `font-mono` with its summary in
`text-xs opacity-60` underneath. The whole row is the label, so clicking
anywhere on it toggles the box. The dialog edits a draft. *Save* sends the
whole list in one `PATCH` (the endpoint takes the whole list, as it does now);
*Cancel*, Escape and clicking outside discard the draft. The dialog is locked
while the request is in flight, and a refusal is shown inside it.

There is no tri-state check option: the only choice is `--nocheck` or nothing.

The add-package dialog does not get flags here. It has never set them, and a
package's flags are a later decision.

### Docs and CLI

- Add a short table of the offered flags to the configuration docs, generated
  by hand from `OFFERED`, with a test that fails when the two differ, as the
  environment variables have.
- Update the `aurcache-cli` `--build-flag` help to say that unknown or
  pointless flags are refused or dropped, and update the test that passes
  `--noconfirm`.
- Update `docs/docs/workers/protocol.md`: workers add the three themselves,
  and protocol-1 servers send them as well.

## Delivery

Two PRs, chained:

1. **Backend**: the four kinds, `OFFERED`, the workers' fixed flags, the
   descriptor keeping them for protocol 1, the startup step and its event, an
   empty default, docs and CLI.
2. **Frontend**: the summary row and the dialog, on top of 1.
