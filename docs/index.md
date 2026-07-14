# Rewind documentation

Rewind records a supervised terminal command and versions the workspace in
which it runs. A recorded checkpoint can be materialized into a new directory
and used as the parent of another run.

> **Implementation status:** the core vertical slice and deterministic demo are
> implemented and locally exercised on Apple Silicon macOS. Documents name
> unverified platform paths and remaining hardening gaps explicitly; a command
> shown for Linux or a future importer is not a compatibility claim.

## Product and architecture

- [Architecture](architecture.md) — boundaries, data flow, state transitions,
  and cross-cutting invariants.
- [Replay semantics](replay-semantics.md) — what record, replay, checkout, and
  fork mean, and what they cannot restore.
- [Storage format](storage-format.md) — local layout, SQLite metadata,
  content-addressed objects, and crash-safety rules.
- [Platform support](platform-support.md) — supported targets, capability
  detection, clone strategies, and verified-host status.
- [Privacy](privacy.md) — capture defaults, sensitive input, exclusions, and
  export behavior.
- [Configuration](configuration.md) — strict TOML locations, precedence,
  defaults, implemented limits, and reserved settings.
- [Threat model](threat-model.md) — trust boundaries, attacks, mitigations, and
  residual risk.

## Planning and development

- [Assumptions](assumptions.md) — explicit implementation constraints and the
  locally verified environment.
- [Roadmap](roadmap.md) — vertical slices and their acceptance criteria.
- [Testing](development/testing.md) — local gates, host-dependent coverage,
  and evidence required before making claims.
- [Local installation](development/installation.md) — local release build,
  completions, manual page, and storage override.
- [Release checklist](development/release-checklist.md) — evidence to gather
  before the repository owner publishes anything.
- [Benchmark methodology](../benches/README.md) — reproducible fixture controls
  and result-reporting rules.

## Architecture decisions

An ADR marked **Accepted** records a design decision. It does not by itself
mean the implementation or every acceptance test is complete.

1. [Workspace isolation and clone strategy](adr/0001-workspace-isolation.md)
2. [Single-writer event ordering](adr/0002-single-writer-event-ordering.md)
3. [Snapshot identity and object storage](adr/0003-snapshot-identity-object-storage.md)
4. [SQLite metadata](adr/0004-sqlite-metadata.md)
5. [Replay is recorded playback](adr/0005-replay-semantics.md)
6. [Narrow platform abstraction](adr/0006-platform-abstraction.md)
7. [Terminal-input privacy](adr/0007-terminal-input-privacy.md)

## Status language

- **Verified**: exercised by a named local or CI command on an identified host.
- **Implemented, unverified**: code exists, but its acceptance path has not run.
- **Design target**: intended behavior; code may not exist yet.
- **Deferred**: deliberately outside the current vertical slice.
