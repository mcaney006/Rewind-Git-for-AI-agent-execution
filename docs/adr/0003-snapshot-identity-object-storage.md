# ADR 0003: Use deterministic trees and file-level content addressing

- **Status:** Accepted
- **Date:** 2026-07-13
- **Implementation:** Implemented with deterministic canonical JSON snapshot
  identity and uncompressed RWOB version 1 objects. Integration tests verify
  creation-order determinism, reuse, restore equivalence, symmetric diffs,
  path/case preflight, and corrupt-object rejection; compression is deferred.

## Context

Checkpoints must compare, deduplicate, restore, export, and survive process
crashes without depending on directory enumeration order or Git. Stored paths
and imported objects are untrusted. Compression must not change object identity.

## Decision

A snapshot is a versioned, canonical tree of normalized relative UTF-8 paths.
Entries encode kind, regular-file content object, symlink target, executable
bit, and supported Unix permissions. Children are sorted by canonical path
bytes before serialization. Scans and restore do not follow symlinks.

`ObjectId` is the BLAKE3 digest of logical uncompressed bytes. Objects use a
two-hex-character fan-out path and are immutable. Publication writes a private
temporary file, flushes it, and atomically installs it without replacing an
existing object. Reads and the internal object-import primitive verify the
digest; bundle import is not implemented. Concurrent publication of the same
logical bytes is success; mismatched existing bytes are corruption.

The initial deduplication unit is a complete file or bounded terminal chunk.
Compression, when enabled for worthwhile objects, is a versioned storage
envelope detail and does not affect identity. Snapshot identity hashes the
canonical versioned tree representation.

## Consequences

- Equal supported workspace state has equal snapshot identity independent of
  enumeration order.
- File-level deduplication is simple and useful for agent-sized edits.
- Non-UTF-8 names and unsupported metadata fail or are reported rather than
  being serialized ambiguously.
- A crash can leave an unreachable object, but metadata never commits a
  reference before object durability.
- Complete validation is required before materialization; object hashes are
  verified again on restore.

## Rejected alternatives

- **Git trees/commits:** couples correctness to Git and mutates repository state.
- **Timestamps or inode numbers in identity:** nondeterministic and nonportable.
- **Follow symlinks:** permits workspace escape and loses link semantics.
- **Content-defined chunking now:** complexity without workload evidence.
- **Writable hard links as deduplication:** violates snapshot immutability.
