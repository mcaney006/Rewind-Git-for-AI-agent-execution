# Architecture

Rewind is a local execution recorder with versioned workspace state. It runs an
arbitrary command in an isolated directory, records the supervised terminal and
process tree, and links deterministic workspace checkpoints to the ordered
execution timeline.

It is not a VM, a security sandbox, or a deterministic process-reexecution
system. See [replay semantics](replay-semantics.md) for the precise boundary.

> **Status:** the recording, checkpoint, checkout, fork, comparison, native
> replay, and export slices are implemented. Verification is host-specific and
> recorded in [testing](development/testing.md); this architecture is not a
> claim of deterministic execution or sandboxing.

## Invariants

1. An event sequence assigned by one recorder coordinator is the authoritative
   order. Wall-clock time is descriptive.
2. The source workspace is not the run working directory, and Rewind never
   uses writable links back to it. Clone traversal pins the root and refuses
   symlink components; scans use the pinned root for regular-file reads. This is
   filesystem isolation for ordinary relative writes, not a sandbox against
   explicit absolute paths or preserved symlink targets.
3. A committed checkpoint refers to a complete immutable snapshot. Readers
   never observe half a checkpoint.
4. Snapshot identity is derived only from supported, deterministically encoded
   workspace state.
5. Stored paths are normalized relative paths. Restore validates the complete
   tree before its first destination write and never follows stored symlinks.
6. File and terminal bytes are immutable content-addressed objects. Metadata
   references them transactionally.
7. Every capture queue is bounded. Backpressure is visible; events are not
   silently discarded.
8. Platform conditionals and unsafe operating-system calls stay behind the
   platform boundary.
9. Library code returns typed errors. Only the CLI chooses presentation and
   process exit codes.

## Component boundaries

The intended dependency direction is downward in this list; cycles are not
allowed.

| Component | Owns | Must not own |
| --- | --- | --- |
| `rewind-domain` | IDs, durable enums, run/checkpoint/event values, state invariants | I/O, SQLite, PTYs, CLI or TUI types |
| `rewind-store` | SQLite migrations and transactions, content-addressed objects, indexed reads, recovery | terminal supervision or filesystem scanning policy |
| `rewind-platform` | capability detection and narrow macOS/Linux filesystem, PTY, terminal, and process primitives | product state transitions or persistence policy |
| `rewind-snapshot` | deterministic scan, path validation, tree encoding, diff, restore | run orchestration or direct CLI output |
| `rewind-capture` | supervised process/PTY lifecycle, bounded producers, coordinator, checkpoint scheduling | argument parsing or presentation |
| `rewind-cli` | configuration resolution, command orchestration, errors and machine-readable output | storage SQL or snapshot algorithms |
| `rewind-tui` | read-only presentation models and paged replay | execution-state mutation |
| `xtask` | reproducible developer and fixture operations | product behavior duplicated outside the public CLI |

The TUI consumes stable paged store reads through its own read-only crate. The
core recorder is synchronous: threads and bounded
`std::sync::mpsc::sync_channel` express the single-writer design without a
runtime-wide dependency.

## Recording flow

```text
source workspace ── safe clone/copy ──► isolated run workspace
                                            │
                                  authoritative initial scan
                                            │
                                      supervised command
                                            │
       ┌─────────── bounded typed messages ─┼────────────┐
       │                    │                │            │
    PTY I/O          process observer   dirty hints   control socket
       └────────────────────┴──────────┬─────┴────────────┘
                                      ▼
                          recorder coordinator
                     sequence → batch → checkpoint
                              │          │
                              ▼          ▼
                           SQLite   object store
```

1. Resolve the source workspace and `REWIND_HOME` once. Reject recursive
   capture or a top-level `.git` indirection file.
2. Pin the source root and create an isolated run workspace through
   descriptor-relative no-follow traversal, using a safe platform clone
   operation or a metadata-preserving recursive-copy fallback.
3. Authoritatively scan that isolated tree and commit it as the run's initial
   snapshot. This makes the snapshot describe the exact tree handed to the
   child rather than a source scan taken before a potentially racing clone.
   Record the selected clone strategy as a typed event.
4. Create the run in `Preparing`, acquire its ownership, then start the command
   under a PTY and transition it to `Running`.
5. Producers send typed observations through bounded channels. Only the
   coordinator assigns sequence numbers and appends events.
6. A descriptor-rooted metadata poller supplies dirty-path hints and schedules
   scans; it never defines truth. Its one-million-entry ceiling disables hints
   visibly rather than risking unbounded memory, while final scanning remains
   enabled. Native watcher integration is deferred. A checkpoint state machine
   prevents overlapping commits.
7. Finalization drains terminal output, performs a complete final scan, flushes
   events, commits the final checkpoint, and stores the terminal outcome. If the
   root exits, Rewind proves its owned process group is empty before the final
   scan even if the PTY already reached EOF. Draining is bounded; Rewind
   requests process-group termination and durably warns before abandoning an
   unavailable tail.

## Run state transitions

Durable transitions are explicit and monotonic:

```text
Preparing ──► Running ──► Completed
    │            ├──────► Failed
    │            ├──────► Interrupted
    │            └──────► Crashed
    ├───────────────────► Failed
    ├───────────────────► Interrupted  (including startup recovery)
    └───────────────────► Crashed
```

`Completed` is valid only from `Running`. Terminal states do not transition
again, and same-state transitions are rejected. A startup recovery transaction
marks records left in `Preparing` or `Running` as interrupted after confirming
no valid owner remains. Recovery does not invent missing events or a final
snapshot.

## Checkpoint state machine

At most one checkpoint commit is active for a run:

```text
Idle ──trigger──► Pending ──scan──► Committing ──success──► Idle
  ▲                  │                 │
  └──coalesce────────┘                 └──failure event──► Idle
```

Manual triggers bypass debounce but not an active commit. Automatic triggers
coalesce within configured minimum/debounce intervals; a maximum interval and
the shutdown path guarantee an eventual scan. The final scan is complete even
when watcher hints were lost.

## Backpressure and bounded resources

- Producer queues have configured finite capacity.
- Terminal output is divided into bounded durable chunks rather than retained
  for the full run.
- Safe producers block when the coordinator is behind. A persistence failure
  terminates capture with a recorded warning/error rather than dropping data.
- Producer backpressure becomes stop-aware during ordered shutdown, and the
  coordinator continues draining terminal observations while joining output.
- Replay reads events and terminal ranges through paged queries and bounded
  caches; a complete recording is not loaded eagerly.
- Object reads validate envelope and physical sizes against an explicit caller
  ceiling before allocation. Export streams objects through EOF so their BLAKE3
  identities are verified without buffering the complete payload.
- Per-file, terminal-output, and per-run unique logical-object limits are
  enforced before durable checkpoint/event references. Bundle size and TUI
  cache limits are enforced at their consumers. A rejected snapshot can leave
  newly published but unreachable objects for garbage collection; the logical
  run limit is not a physical store quota.
- Authoritative scans currently run synchronously. During a large checkpoint,
  the bounded PTY queue can backpressure the child instead of dropping output.

An explicitly lossy capture mode is deferred. If introduced, it must be opt-in
and permanently marked on the run.

## Locking and consistency

The first implementation uses one store-wide nonblocking kernel advisory lock
held through a no-follow regular-file descriptor. Its permission-restricted
file persists only for bounded owner diagnostics; descriptor close, including
process termination, releases ownership automatically. This deliberately
serializes recordings, forks, and garbage collection while SQLite WAL and
immutable objects permit independent readers. Checkout and replay open the
store read-only. Bundle import is not exposed by the CLI. Per-run writer locks
are deferred until measured concurrent-run demand warrants their additional
recovery rules.

Objects are persisted before a transaction commits references to them. A crash
may therefore leave unreachable objects, but never a committed reference to an
unwritten object. The final `RunCompleted` batch and terminal run transition
commit in one transaction. Under the writer lock, garbage collection validates
typed event payloads against denormalized kinds before combining indexed graph
reachability with a bounded physical scan for digest-shaped unindexed objects
and object-publication `.tmp-*` artifacts; it never infers reachability from
timestamps.

## Shutdown order

Normal and interrupted exits share one structured cleanup path:

1. stop accepting control messages;
2. stop scheduling new checkpoints;
3. request child termination when necessary;
4. drain PTY output and reap the child; after root exit, bound the drain,
   terminate its remaining process group, and warn if a terminal tail must be
   abandoned;
5. commit the authoritative final checkpoint when possible;
6. atomically append pending terminal events and persist the terminal run status;
7. explicitly restore the parent terminal, retaining the RAII guard as fallback;
8. surface any restoration failure without hiding the primary capture error;
9. release run and store ownership.

Failures are accumulated with the primary failure retained. Terminal
restoration and child reaping are attempted on every exit path.

## Branching and comparison

A fork has one immutable parent edge: child run → parent run → parent
checkpoint → parent snapshot. Fork materialization creates a new directory and
cannot mutate any ancestor workspace or snapshot. The persistence boundary
rejects cycles even though the public creation path only adds a child of an
existing checkpoint.

Comparison derives evidence from recorded metadata and snapshot trees: ancestry,
file changes, executable-bit changes, exit status, duration, terminal size,
checkpoint/warning counts, and optional evaluation results. Evaluation commands
run in separate materializations of each final snapshot. Rewind does not invent
a winner or synthesize a quality score.

## Deferred until the core path is measured

Content-defined chunking, a daemon, cloud synchronization, distributed stores,
runtime plugin discovery, and deterministic process re-execution are outside
the current architecture. Native watchers are optimizations after authoritative
scan correctness, not prerequisites.
