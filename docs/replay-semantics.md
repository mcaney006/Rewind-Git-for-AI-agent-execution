# Replay semantics

Rewind versions local workspace state and execution traces. It does not restore
process memory or undo external side effects.

> **Status:** these semantics are the product contract. Fork, noninteractive
> replay, and bundle/HTML export have local end-to-end coverage; the native TUI
> has paged-model/parser tests but not automated interactive terminal coverage.

## Terms

### Record

`rewind run -- <command>` supervises one root command and observes its
descendants, terminal traffic, lifecycle events, and mutations within an
isolated workspace. Rewind preserves raw terminal bytes and typed event
metadata; it does not interpret hidden model reasoning.

Process-tree metadata is observational. Polling can miss short-lived children,
and an executable path or exit status may be unavailable on a supported host.
The supervised root process and PTY outcome remain the run boundary.

### Checkpoint

A checkpoint links a point in the ordered event timeline to an immutable
snapshot of supported workspace state. Sequence number is authoritative;
timestamps and monotonic offsets support presentation only.

Filesystem notifications are dirty-path hints. A checkpoint is produced by
reading filesystem state, and the final checkpoint uses a complete scan. A
process can still mutate a file while it is being scanned, so a checkpoint is a
best-effort observed filesystem state rather than a kernel-atomic freeze.

### Replay

Replay is playback of recorded terminal and execution events synchronized with
recorded checkpoints. It can seek, pause, change playback speed, and inspect the
workspace represented by a checkpoint without rerunning the command.

The native workspace pane computes file changes from snapshot manifests and
loads content only for the selected entry. Text previews are capped at 256 KiB
per side and 400 preview lines; larger, binary, and non-UTF-8 content remains
identified but is not rendered as a unified diff. `PageUp` and `PageDown`
navigate the selected preview, and carriage returns are rendered visibly so a
CRLF-to-LF change is not obscured.

Replay does **not** imply:

- deterministic re-execution;
- restoration of a process address space, open descriptors, or kernel state;
- recreation of remote services, external databases, network responses, or
  wall-clock conditions;
- capture of unrelated machine processes or files outside the workspace;
- access to private chain-of-thought or vendor-private session formats.

### Checkout

Checkout materializes one checkpoint snapshot into an explicit destination.
The complete stored path set is validated first. Rewind refuses a nonempty
destination without an explicit destructive option and builds in a temporary
sibling before rename when the filesystem permits it.

Checkout never changes the recorded run, the parent snapshot, or the original
source workspace. It restores supported file contents, symlinks, executable
bits, and Unix permissions; unsupported metadata is reported rather than
silently claimed.

### Fork

Fork is checkout plus a new recording:

1. resolve and verify the selected checkpoint;
2. materialize its snapshot into a new isolated run workspace;
3. create a child run whose parent is that exact run/checkpoint pair;
4. start a new supervised command in the child workspace.

A fork does not resume the parent process. Environment, network, time, and
third-party state may differ. Parent and child share immutable objects where
their content is equal but never share writable hard links.

## Ordering and timing

Each persisted event contains a run ID, schema version, strictly increasing
sequence, wall-clock timestamp, monotonic offset, and typed payload. Sequence
defines total order within a run. Two events with equal or surprising wall-clock
times retain their sequence order.

Playback timing uses monotonic offsets and may scale delays. Seeking to an event
positions terminal playback and the nearest applicable checkpoint; it does not
fabricate filesystem states between committed checkpoints.

## Terminal bytes and safety

Recorded terminal output retains ANSI escape bytes for faithful native replay.
It is untrusted display data. Native presentation must constrain escape
handling, and HTML export must parse supported ANSI formatting and HTML-escape
all text rather than inject raw terminal bytes.

When stdout is redirected, `rewind replay` is an exact raw-byte stream suitable
for files or another explicit consumer. Rewind refuses that raw fallback when
stdout is a terminal but stdin is unavailable for the constrained TUI.

Input recording follows the policy in [Privacy](privacy.md). A redacted input
event preserves timing and byte length without the bytes themselves.

## Incomplete runs

A recorder crash can leave a run without a final checkpoint or terminal event.
Startup recovery marks such a run interrupted; replay presents the durable
prefix and an explicit warning. Recovery does not infer an unrecorded ending.

## Comparison semantics

Comparison reports evidence, not a replay-quality score: ancestry, start and
final snapshots, file changes, exit status, duration, terminal-output size,
checkpoints, warnings, and optional evaluation results. An evaluation command
runs separately in isolated materializations of both final snapshots. Its
command, status, and elapsed time are reported verbatim; it is not executed in
the source workspace.
