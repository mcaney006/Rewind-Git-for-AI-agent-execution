# ADR 0004: Store indexed metadata in SQLite WAL

- **Status:** Accepted
- **Date:** 2026-07-13
- **Implementation:** Implemented as schema version 2; tests verify empty and
  version 1 migrations, rollback on migration failure, concurrent read-only
  access, repository round trips, and startup recovery. Process-kill fault
  injection remains pending.

## Context

Run timelines require transactional relationships, ordered range queries,
ancestry, recovery, and concurrent read-only inspection. Large file and terminal
bytes should not inflate the transactional database or defeat deduplication.

## Decision

Use one local SQLite database in WAL mode for indexed metadata and a separate
immutable content-addressed object store for large bytes. Enable foreign keys on
every connection. Core records use typed columns and explicit schema versions,
not opaque unversioned JSON.

Repository operations are product transactions such as `create_run`,
`append_event_batch`, `commit_checkpoint`, `finish_run`, `load_timeline`, and
`mark_interrupted_runs`; callers do not issue SQL. A checkpoint transaction
links only already-durable objects. Applied numbered migrations are immutable
and run transactionally.

The first mutation model uses an atomic store-wide writer lock. SQLite WAL
allows independent readers during a recording. Startup recovery marks abandoned
`Preparing`/`Running` runs interrupted through a documented transaction.

## Consequences

- Foreign keys, unique constraints, and transactions enforce much of the graph.
- Timeline and terminal-range indexes are added only for actual query shapes.
- Object publication and metadata commit have an explicit crash boundary;
  unreachable objects are safe for later reachability-based GC.
- Read paths still validate invariants because malformed database content is an
  untrusted input.
- Concurrent recorders serialize initially; per-run writer locks are deferred
  until demand justifies their recovery complexity.

## Rejected alternatives

- **Filesystem metadata files only:** difficult atomic graph updates and range
  queries.
- **Every byte in SQLite:** database/WAL bloat and poor cross-run deduplication.
- **A daemon database service:** adds setup and failure modes to a local tool.
- **Unversioned JSON rows:** weak constraints and ambiguous migration semantics.
- **Edit migrations in place:** makes existing stores unreproducible.
