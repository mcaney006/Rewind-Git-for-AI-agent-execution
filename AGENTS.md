# Rewind repository guide

Rewind runs arbitrary coding commands inside branchable, replayable local
workspaces. It records a supervised process tree, terminal events, and
versioned workspace state; it is not a VM or deterministic process replay.

## Repository map

- `crates/rewind-domain`: durable types and invariants; no I/O dependencies.
- `crates/rewind-store`: SQLite metadata and content-addressed objects.
- `crates/rewind-platform`: narrow macOS/Linux filesystem, PTY, and process APIs.
- `crates/rewind-snapshot`: deterministic scan, diff, and safe materialization.
- `crates/rewind-capture`: bounded recorder coordination and checkpoints.
- `crates/rewind-tui`: read-only native replay presentation.
- `crates/rewind-cli`: argument parsing and orchestration.
- `xtask`: reproducible checks, fixtures, demo, and packaging preparation.
- `docs/index.md`: documentation map and architectural decisions.

## Invariants

- Event sequence, not timestamp, is authoritative.
- One bounded coordinator assigns sequences and persists event metadata.
- Terminal/file bytes are immutable content-addressed objects.
- Snapshot identity is deterministic; final scans are authoritative.
- Restore validates every path before writing and never follows stored symlinks.
- Default runs use isolated workspaces without writable links to the source;
  this protects ordinary relative writes but is not a process sandbox.
- No library calls `process::exit`; only the CLI maps errors to exit codes.
- No terminal input or file content is written to tracing logs.

## Development

Run `cargo xtask check` before handing off ordinary changes and
`cargo xtask test-all` for the complete local suite. Use `cargo fmt --all` and
`cargo clippy --workspace --all-targets -- -D warnings` directly while iterating.
See `docs/development/testing.md` for host-dependent checks.

Schema changes require a numbered transactional migration plus empty-database
and upgrade tests; never edit an applied migration. Target-specific `cfg` and
FFI stay inside `rewind-platform`; narrow portable `cfg(unix)` metadata guards
may remain beside persistence code. Unsafe code is denied elsewhere. Every
unsafe block requires a `SAFETY` comment and a focused safe-wrapper test.

Do not create commits, tags, releases, authorship trailers, or assistant/model
attribution. Preserve existing ownership and leave publication to the repository
owner.
