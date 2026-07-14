# Rewind

Run any coding agent inside a branchable, replayable workspace.

```bash
rewind run -- codex
```

Rewind records an arbitrary terminal command and the isolated workspace it
changes. You can navigate its ordered timeline, materialize an earlier
checkpoint, start another command from that point, and compare filesystem and
test evidence without changing the source checkout.

The repository includes a local breaker/fixer demo. This is an excerpt from an
actual `cargo xtask demo` run on macOS; UUIDs and timings vary:

```text
Status        failed <> completed
Exit          code 1 <> code 0
Checkpoints   3 <> 2
Changed files 1
  Modified { content_changed: true, ... }  calculator.sh
Evaluation    ./test.sh
  left        code 1 in 470ms
  right       code 0 in 438ms
```

See the [captured comparison transcript](docs/demo-transcript.txt). The command
leaves both runs and their branch relationship available for native replay.

> **Status:** the core recording, checkpoint, checkout, fork, compare, native
> replay, bundle/HTML export, doctor, and dry-run GC paths are implemented and
> exercised on Apple Silicon macOS. No package has been published. Linux is a
> supported code path with CI configured, but it has not been runtime-verified
> in this checkout.

## Quick start

Build from this repository with stable Rust:

```bash
cargo build --release --locked --bin rewind
./target/release/rewind doctor
./target/release/rewind run -- cargo test
./target/release/rewind replay
```

Rewind uses the platform application-data directory automatically. Set
`REWIND_HOME=/explicit/path` for an isolated store. No database server,
container, agent adapter, or vendor account is required.

Run the complete account-free demo with:

```bash
REWIND_DEMO_DIR=/tmp/rewind-demo cargo xtask demo
```

The final lines print exact replay commands and the retained data location.

## What Rewind records

- raw terminal output and ANSI bytes from a real pseudoterminal;
- policy-controlled terminal input, with no-echo input redacted by default;
- typed events ordered by one monotonically increasing sequence;
- observational metadata for the supervised root process and descendants;
- initial, quiescent, manual, and final workspace checkpoints;
- run status, exit evidence, duration, warnings, ancestry, and clone strategy.

Filesystem changes are detected by descriptor-rooted, bounded metadata polling.
An oversized hint scan disables itself visibly. Those dirty paths are hints;
every checkpoint reads authoritative filesystem state, and
finalization performs a complete scan. Sequence order—not wall-clock time—is
authoritative. Rewind does not capture hidden model reasoning or require
vendor-private formats.

Agent support is stated at three distinct levels:

| Support level | Current state |
| --- | --- |
| Generic terminal capture | implemented for arbitrary commands |
| Structured metadata integration | no vendor adapter currently ships |
| Validated compatibility | no vendor-specific certification is claimed |

## Rewind and fork a run

```bash
rewind run -- sh -c 'edit-and-test'
rewind list
rewind show <run>
rewind replay <run>
rewind checkout <run>@<checkpoint> --to ./inspected-workspace
rewind fork <run>@<checkpoint> -- claude
```

Runs and forks execute in separate Rewind-owned workspaces. Checkout requires
an explicit destination and refuses nonempty content unless `--force` is used.
A fork stores an immutable parent edge to the exact run/checkpoint selected; it
does not resume process memory.

While a run is active, another shell can create an immediate checkpoint:

```bash
rewind mark "before auth refactor"
```

## Compare two agents

Comparison works without an agent adapter:

```bash
rewind compare <original-run> <forked-run>
rewind compare <original-run> <forked-run> --test 'cargo test'
rewind compare <original-run> <forked-run> --json
```

Rewind reports ancestry, starting and final snapshots, file changes,
executable/permission changes, run status, exit status, duration, terminal
bytes, checkpoints, warnings, and optional evaluation results. Each evaluation
runs in its own materialized final snapshot, never in the source workspace.
There is no invented winner or aggregate quality score.

## How it works

```text
source workspace ── safe clone/copy ──► isolated run workspace
                                               │
                                      supervised PTY command
                                               │
                                bounded single event writer
                                  ┌────────────┴────────────┐
                                  ▼                         ▼
                           SQLite WAL metadata       immutable objects
                                  │                         │
                                  └────── checkpoint ───────┘
                                               │
                                  replay / checkout / fork / compare
```

SQLite stores indexed metadata. Regular-file and bounded terminal bytes use a
BLAKE3 content-addressed object store. Snapshot identity hashes deterministic,
path-sorted JSON over supported state. APFS `fclonefileat` and Linux `FICLONE`
accelerate workspace isolation when available; a metadata-preserving recursive
copy is the safe fallback. Writable hard links are never used as workspace
copy-on-write.

Native replay uses paged timeline reads, a bounded terminal-object cache, and
bounded unified text previews for the selected workspace change.
Noninteractive replay streams recorded terminal bytes. Export can create a
self-contained offline HTML replay or a deterministic, checksummed RWBN bundle.

See the [documentation map](docs/index.md),
[architecture](docs/architecture.md),
[configuration](docs/configuration.md), and
[storage format](docs/storage-format.md).

## Privacy

Rewind is local-only: no telemetry, analytics, cloud sync, background upload,
or crash reporting. Environment values are not captured. Terminal input
defaults to best-effort echo-aware redaction sampled beside each input read;
use `--record-input=never` for sensitive sessions because echo detection can
still race.

Common secret filenames are excluded from snapshots by default and removed
from the retained isolated workspace during orderly finalization. That is not
secure erasure, and a crash before cleanup can leave bytes in the private run
workspace. Terminal output and ordinary included files can still contain
secrets. Review [privacy](docs/privacy.md) and the
[threat model](docs/threat-model.md) before sharing a store or export.

## Platform support

| Platform | Status |
| --- | --- |
| macOS Apple Silicon | locally verified on macOS 26.6, case-insensitive APFS |
| Linux x86-64 | implemented; CI configured; runtime result not yet observed here |
| Linux ARM64 | implemented; CI configured; runtime result not yet observed here |
| Windows and other targets | rejected at compile time for the first release |

The macOS verification covered APFS cloning, PTY capture, process observation,
snapshots, checkout, fork, compare/evaluation, replay streaming, export, and the
fixture demo. See [platform support](docs/platform-support.md) for the exact
boundary.

## Limitations

Rewind versions local workspace state and execution traces. It does not restore
process memory or undo external side effects.

It does not restore kernel state, remote services, external databases, network
responses, unrelated processes, or files outside the workspace. A checkpoint
scan is not an atomic filesystem freeze. Process polling can miss short-lived
descendants. Rewind is not a security sandbox; the child retains the launching
user's operating-system permissions.

The initial snapshot model supports regular files, symlinks, executable bits,
and Unix permissions. Non-UTF-8 names and unsafe paths are rejected. Extended
attributes, ACLs, ownership, sparse layout, and other platform-specific metadata
are outside the portable identity. Native filesystem watchers, bundle import,
environment capture, binary-file classification, systematic process-kill fault
injection, and cross-run physical disk quotas remain incomplete. The per-run
quota bounds unique logical referenced object bytes, not retained workspace or
CAS envelope size.

## Development

The workspace uses Rust 2024 and the current stable toolchain. Ordinary gates
are:

```bash
cargo xtask check
cargo xtask test-all
cargo xtask demo
cargo xtask package
```

See [testing](docs/development/testing.md), the
[roadmap](docs/roadmap.md), and the
[release checklist](docs/development/release-checklist.md). Benchmark results
must name the command, fixture, host, filesystem, and sample count; this README
makes no portable performance claim.

Commits, tags, releases, and publication are left to the repository owner.
