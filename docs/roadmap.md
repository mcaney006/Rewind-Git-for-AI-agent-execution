# Roadmap

Rewind is built as end-to-end slices. A phase is complete only when its public
command and failure paths are exercised; empty handlers do not count.

Status on 2026-07-13: phases 1–4 are locally verified on macOS. Phase 5 is
implemented with noninteractive/export verification and model-level TUI tests;
automated interactive TUI coverage and bundle import remain. Phase 6 has the
demo, bounded fuzz targets, benchmark harness, GC, and focused recovery tests,
but the systematic crash-fault matrix and Linux runtime evidence remain.

## 1. Foundation — locally verified

- Rust 2024 workspace, lint policy, concise repository instructions, CI, and
  reproducible `cargo xtask` checks.
- Durable domain types and state transitions.
- Strict configuration and narrow platform capability detection.
- Architecture, storage, privacy, threat-model, and semantic ADRs.

Acceptance: `cargo xtask check` and `rewind doctor` succeed.

## 2. Recording vertical slice — locally verified

- Initial snapshot, isolated clone/copy, PTY supervision, bounded event writer,
  streamed terminal chunks, root/process observation, clean shutdown, and
  interrupted-run recovery.
- `init`, `run`, `list`, and `show` with text and JSON output.

Acceptance: recording `sh -c 'printf "hello\\n"; exit 7'` preserves output and
exit status while leaving the source workspace unchanged.

## 3. Checkpoints and restore — locally verified

- Deterministic content-addressed snapshots, manual control-socket markers,
  periodic quiescent checkpoints, authoritative final scan, safe restore, and
  object reuse.
- `mark` and atomic `checkout`.

Acceptance: changed, added, deleted, executable, and symlink entries round-trip;
malicious paths cannot escape the destination.

## 4. Branch and compare — locally verified

- Durable parent/checkpoint links, cycle rejection, arbitrary-depth run trees,
  isolated forks, snapshot diffs, and optional isolated evaluation commands.
- `fork` and `compare` in text and JSON forms.

Acceptance: a child starts from a mid-run checkpoint, the parent stays unchanged,
and both final snapshots can be evaluated independently.

## 5. Replay and export — implemented, interactive coverage incomplete

- Paged read APIs and a restrained native TUI with branch, timeline, terminal,
  workspace, process, details, seeking, and discoverable keys.
- Versioned bundle export and a self-contained, safely escaped offline HTML
  replay artifact.

Acceptance: the deterministic fixture is navigable without loading the complete
terminal recording into memory, and exported output works without a server.

## 6. Hardening and demo — in progress

- Safe dry-run garbage collection, crash recovery tests, property tests, fuzz
  targets, realistic benchmarks, packaging checks, and release documentation.
- Local breaker/fixer agents driven through the public CLI by `cargo xtask demo`.

Acceptance: the demo records a failure, forks before the bad change, records a
successful fix, compares evidence, and leaves replayable data for screenshots.

Remaining hardening work includes process-kill injection at each persistence
and shutdown boundary, bundle graph import/staging, native watcher integration,
interactive TUI automation, Linux runtime evidence, coverage collection, and a
physical store quota distinct from the per-run logical reference limit.

## Explicitly deferred until evidence warrants it

- Content-defined chunking, a daemon, cloud synchronization, plugin discovery,
  distributed storage, deterministic process re-execution, and a single
  synthetic quality score.
