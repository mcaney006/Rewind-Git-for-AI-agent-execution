# ADR 0005: Define replay as recorded playback, not re-execution

- **Status:** Accepted
- **Date:** 2026-07-13
- **Implementation:** Native read-only replay is implemented with paged
  timelines, a bounded terminal-object cache, and on-demand workspace diffs.
  Parser/cache unit tests exist; automated end-to-end interactive TUI coverage
  remains pending.

## Context

“Replay” can imply deterministic process execution or machine checkpointing.
Rewind observes an ordinary command but cannot capture kernel state, remote
responses, process memory, or every source of nondeterminism. Claiming otherwise
would make forks and comparisons misleading.

## Decision

Replay presents the durable sequence of terminal/execution events alongside
committed workspace checkpoints. It supports playback timing, seek, step, and
checkpoint inspection without rerunning the original process.

Checkout materializes supported filesystem state. Fork materializes a selected
checkpoint into a new isolated workspace and starts a new command with a durable
parent edge. It does not resume the parent process.

Incomplete runs replay their durable prefix and show an interruption warning.
Comparisons report recorded evidence and optional isolated test results; Rewind
does not invent a winner or quality score.

## Consequences

- The product promise is achievable for arbitrary terminal commands without
  vendor-private formats.
- External side effects and nondeterministic dependencies are not reversible.
- Filesystem checkpoints are observed scans, not atomic kernel snapshots.
- Documentation and UI must use “recorded playback,” “materialize,” and “fork”
  precisely and display missing/incomplete data.

## Rejected alternatives

- **Deterministic process replay:** requires kernel/runtime control beyond the
  product boundary and still cannot undo remote effects.
- **Re-execute on replay:** changes history and can repeat destructive effects.
- **Call checkout full restoration:** ignores process and external state.
- **Vendor session replay as the core:** excludes arbitrary commands and relies
  on unstable private formats.
