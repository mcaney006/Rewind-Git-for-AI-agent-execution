# Testing and developer checks

Tests are evidence for a capability claim. This page distinguishes the checks
that run in the ordinary gate from tool-dependent and still-missing coverage.

> **Local tool status (2026-07-13):** Rust/Cargo 1.96.1, rustfmt, and Clippy are
> installed on an Apple Silicon macOS 26.6 host. `cargo-deny`, `cargo-fuzz`,
> `cargo-nextest`, and `cargo-llvm-cov` are not installed. Linux is not locally
> verified.

## Fast gate

The ordinary handoff command is:

```bash
cargo xtask check
```

It must run, without hiding subprocess output:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --lib
```

`xtask` streams each child command's output and stops at the first failure. Do
not report the aggregate gate as passing merely because one constituent command
passed.

## Full local gate

```bash
cargo xtask test-all
```

This runs the workspace test suite, including the current public-CLI and capture
integration tests, then documentation tests. It includes deterministic invariant
loops that cover the snapshot properties below; the project does not currently
pull in a separate property-test framework. Host-unavailable coverage must be
reported as skipped, not passed.

The developer interface also reserves these explicit operations:

```bash
cargo xtask test
cargo xtask lint
cargo xtask coverage
cargo xtask bench
cargo xtask demo
cargo xtask doctor
cargo xtask package
```

`coverage` fails with an actionable message when `cargo-llvm-cov` is absent.
`bench` runs the explicitly feature-gated, dependency-free harness. `package`
builds locally and generates completions and a manual page; it does not publish
anything. No xtask downloads tools or uploads artifacts.

## Test layers

### Unit tests

Keep invariants next to their owner:

- domain ID parsing, durable enum decoding, event sequences, and run transitions;
- strict size/duration configuration and unknown-field rejection;
- object envelope validation and checkpoint policy;
- stored-path normalization and destination preflight;
- comparison classification and privacy-policy decisions.

### Property-style invariant tests

Use generated or deliberately reordered inputs for invariants with large state
spaces:

- tree identity is independent of insertion/enumeration order;
- storing identical bytes is idempotent;
- supported trees round-trip through snapshot and restore;
- diff direction swaps additions/deletions and preserves modifications;
- valid event batches remain strictly ordered;
- malicious paths never escape the target;
- bundle framing round trips in deterministic path order and rejects unsafe
  paths or invalid checksums.

A fixed regression case accompanies any property-test failure before its seed is
discarded.

### Integration tests

One public-CLI test invokes the built `rewind` binary with temporary
`REWIND_HOME` and source directories. It covers an isolated nonzero run, exact
terminal replay, final checkout, initial-checkpoint fork, ancestry comparison,
isolated evaluation of both final snapshots, and an unchanged source workspace.

Separate library integration tests exercise real PTY capture, exact raw output,
manual control-socket markers, exclusion and size policy, quiescent checkpoints,
run limits, symlinks/modes, restore refusal, and a descendant retaining a PTY
after root exit. The deterministic demo covers the public breaker/fixer
workflow. Interactive input/resize, CLI interruption, descendant process-event
coverage, ANSI-specific output, and every individual CLI error path are not
automated integration claims.
Timing-sensitive cases use bounded process synchronization; unit tests avoid
arbitrary sleeps where the state machine can be exercised directly.

### Crash tests

The required failure-injection matrix is:

1. before run creation completes;
2. during object publication;
3. during checkpoint transaction;
4. after object durability but before metadata commit;
5. during finalization and terminal-chunk flush;
6. during temporary-tree materialization.

Migration rollback, atomic event batches, object publication, interrupted-run
recovery, materialization rollback, kernel-lock release-on-drop, and physical
GC artifact cleanup have focused tests. The complete kill-at-each-shutdown-phase
matrix above is not automated and remains a release blocker. Unreachable
immutable objects are acceptable; dangling committed references are not.

### Fuzzing

Fuzz targets compile independently for control-frame, object-envelope, bundle,
archive-path, and typed event JSON decoders. Every target imposes a per-case
input/resource ceiling before parsing. `cargo-fuzz` is a separate
tool-dependent gate and is not silently folded into ordinary unit tests.

### Benchmarks

The current benchmark harness measures many-small-file initial scan, a small
changed subset with object deduplication, materialization, transactional
terminal-frame ingestion, and bounded timeline paging. Results record date,
commit supplied by the repository owner, OS, architecture, filesystem, Rust
version, fixture shape, sample count, and command. Checkpoint and comparison
benchmarks remain future additions.

No performance number belongs in README until the benchmark has actually run
on identified hardware. Content-defined chunking is considered only if those
fixtures demonstrate material benefit.

## Deterministic demo verification

`cargo xtask demo` uses the public CLI against a fresh explicit store:

1. generate the small fixture repository;
2. run the breaker agent and retain its failing result;
3. choose the checkpoint immediately before the bad mutation;
4. fork the fixer agent from that checkpoint;
5. compare final filesystem and test evidence;
6. print exact replay commands and retain the data.

Output checked into docs must come from an actual clean run, not hand-authored
imitation.

## Platform coverage

The checked-in workflows run formatting, Clippy, workspace tests, and
documentation tests on Linux stable Rust. A platform matrix runs workspace
tests on Ubuntu x86-64, Ubuntu ARM64, and macOS, subject to the repository
owner enabling those workflows. Until a workflow result exists, this is CI
configuration rather than verified platform evidence.

Platform tests report the selected clone strategy. A recursive-copy pass is not
evidence that APFS clone or Linux reflink paths work.

## Release-candidate evidence

Before publication, record results for:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace --doc
cargo xtask test-all
cargo xtask demo
```

Also record a harmless real PTY capture, checkout, fork, compare with evaluation,
and an interrupted-run recovery. Search source for `TODO`, `FIXME`, `todo!`,
`unimplemented!`, empty handlers, fixture-only product paths, and prohibited
authorship text. Inspect the complete diff and confirm that no commit or
publication action was performed.
