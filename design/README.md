# Designs

One document per change worth thinking through before it is built, in the
folder that says where it stands:

- `proposed/` -- written, not started. Revised in place while it is
  discussed.
- `in-progress/` -- being built. Partly true of the code, and kept current
  with it.
- `implemented/` -- built. Each says in its status line what was left out on
  purpose. A design later replaced stays here, marked superseded, for its
  reasoning.
- `rejected/` -- decided against, or postponed, with what would reopen it.
- `reviews/` -- reviews of the code or of a design, rather than designs: a
  snapshot of what was found, and what was done about it.

Every design opens with a status line, `Status: **Proposed** · Last updated:
YYYY-MM-DD`, and moves folders as that status changes. A design's references
to code (`file.rs:123`) describe the code when it was written; check them
before relying on one.
