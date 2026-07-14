# ADR 0001: Isolate run workspaces and accelerate safe copies

- **Status:** Accepted
- **Date:** 2026-07-13
- **Implementation:** Implemented for generic capture; a real-PTY integration
  test on macOS verifies source immutability, retained isolated changes, and
  recorded clone strategy. Linux runtime verification remains pending.

## Context

Arbitrary commands can modify, delete, or rename any workspace file. Running a
coding agent in the user's current checkout would make “record” destructive and
would make a failed fork impossible to separate from the source. Rewind must
also work outside Git repositories and cannot represent versions as Git commits.

## Decision

Default `rewind run` performs an authoritative initial snapshot and executes in
a new run-owned workspace outside the source tree.

Source traversal is descriptor-relative and refuses symlink components.
Regular files use the fastest safe available per-file strategy from a pinned
descriptor: APFS `fclonefileat(2)` on macOS, `FICLONE` reflinks on Linux, then a
metadata-preserving recursive copy. Symlinks are recreated from `readlinkat`
data without following them. Writable hard links are prohibited. The selected
strategy is durable run metadata and copy fallback is visible to the user.

A normal `.git` directory is included so generic Git-aware commands continue to
work. A top-level `.git` indirection file is rejected initially because it can
point at shared worktree metadata outside isolation. Snapshotting itself remains
independent of Git.

The resolved Rewind storage root must not lie within captured content. An
advanced in-place mode is deferred until isolated capture is complete and, if
added, must require an explicit warning-bearing option.

## Consequences

- Default runs cannot mutate the source workspace through ordinary relative
  workspace writes.
- Forks and evaluations receive separate materializations and never writable
  links to parent state.
- Copy fallback can cost time and disk space; the warning is honest rather than
  trading correctness for hidden exclusions.
- Commands that require a linked Git worktree are unsupported initially.
- This is filesystem isolation, not a process sandbox; a malicious command can
  still access paths allowed by the user's operating-system account.

## Rejected alternatives

- **Run in place:** violates the primary safety invariant.
- **Always create a Git worktree:** excludes non-Git inputs and can share
  mutable repository metadata.
- **Commit every checkpoint:** mutates Git state and cannot capture arbitrary
  workspaces cleanly.
- **Writable hard-link farm:** parent and child writes alias each other.
- **Container or VM:** adds a different isolation/product contract and is not
  required for branchable workspace state.
