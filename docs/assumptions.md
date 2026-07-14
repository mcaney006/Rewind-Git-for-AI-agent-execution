# Assumptions

These assumptions bound the first Rewind implementation. They are deliberately
specific so unsupported behavior fails clearly instead of looking reliable.

## Verified environment

- The initial development host is macOS 26.6 on Apple Silicon (`arm64`) with a
  case-insensitive APFS volume.
- Rust and Cargo 1.96.1 are installed and support Rust 2024. The active binaries
  come from Homebrew; the installed rustup toolchain currently has the same
  version.
- Apple Command Line Tools, `fclonefileat(2)`, PTYs, `libproc`, and SQLite are
  available. Full Xcode and Cargo extensions such as `cargo-deny`, `cargo-fuzz`,
  and `cargo-nextest` are not installed.
- Linux behavior will be implemented behind the same platform boundary and
  exercised in CI; Linux is not locally verified on this host.

## Product boundary

- Rewind versions one local workspace plus events from one supervised process
  tree. It is not a VM, security sandbox, or deterministic process replay
  engine.
- Replay means ordered playback of recorded terminal and execution events
  alongside workspace checkpoints. It does not re-execute the process.
- Restoring a checkpoint cannot restore process memory, kernel state, remote
  services, external databases, network effects, or files outside the selected
  workspace.
- Commands are arbitrary executables. Vendor adapters may add explicitly
  exposed metadata later, but generic PTY capture is the compatibility floor.

## Workspace and filesystem

- `rewind run` operates in an isolated workspace by default, so ordinary
  relative writes do not touch the source workspace. Rewind is not a sandbox:
  a command can still use explicit absolute paths or accessible symlink targets.
- A normal `.git` directory is copied so Git-aware commands continue to work.
  A top-level `.git` indirection file is rejected initially because following
  it could let an isolated command mutate shared worktree metadata.
- No dependency or build-cache directory is ignored by default. Correct command
  behavior is preferred over faster but incomplete snapshots; users may add
  explicit ignore rules.
- Stored snapshot paths must be relative, normalized UTF-8 paths. Non-UTF-8
  names are rejected rather than serialized ambiguously. Symlinks are recorded
  without being followed.
- APFS is case-insensitive on the verified host, so materialization rejects
  case-folding path collisions before writing.
- macOS uses per-file APFS clones when possible. Linux uses reflinks when
  possible. Both fall back to a metadata-preserving recursive copy; writable
  hard links are never used.

## Persistence and concurrency

- UUIDv7 identifiers provide sortable, locally generated run/checkpoint/event
  IDs. Snapshot and object identity use BLAKE3.
- SQLite WAL stores indexed metadata; immutable file and terminal bytes live in
  a content-addressed object store.
- Event sequence is authoritative. Wall-clock timestamps are descriptive only.
- One bounded coordinator assigns event sequences and writes event metadata.
  Backpressure blocks producers; events are never silently discarded.
- The first implementation uses one store-wide mutation lock while recordings,
  forks, checkouts, imports, or garbage collection mutate storage. SQLite WAL
  still permits independent read-only inspection. Per-run locks are an upgrade
  only when measured concurrent-run demand justifies them.
- Filesystem notifications and process polling are observational hints. Every
  final checkpoint performs an authoritative scan.
- Terminal input echo-state detection is best effort. `--record-input=never` is
  the documented safe choice for sensitive prompts.

## Delivery

- The repository began with only a two-line README and pre-existing local IDE
  state; there is no implementation or convention to preserve.
- The pre-existing aggregate `.gitignore` hides nearly the whole requested
  project and must be replaced with a focused Rust ignore file.
- Dependencies may be downloaded from their normal package registry during
  Cargo resolution; no third-party project implementation will be copied.
- No performance or platform claim will be published without a recorded run on
  identified hardware.
- The repository owner alone creates commits, authorship metadata, tags,
  releases, and publications.
