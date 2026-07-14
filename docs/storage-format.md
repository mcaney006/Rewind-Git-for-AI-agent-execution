# Storage format

Rewind separates indexed metadata from immutable bytes. SQLite stores runs,
events, checkpoints, snapshot entries, and object references. A BLAKE3-addressed
object directory stores file and bounded terminal bytes.

> **Implementation status (2026-07-13):** SQLite schema version 2, RWOB version
> 1 objects, canonical snapshot JSON, transactional run/event/checkpoint
> operations, startup recovery, RWBN version 1 framing, and bundle export are
> implemented. Object consumers use explicit read ceilings or verified
> streaming. Focused tests cover migrations, object integrity, repository
> invariants, deterministic scan/restore, and the bundle codec on the current
> macOS host. Linux runtime behavior and systematic crash-fault injection are
> not verified; bundle import is not implemented. The pre-1.0 formats are not
> yet compatibility promises.

## Store root

`REWIND_HOME` overrides the platform application-data location. The resolved
path is fixed for a process and must not be recursively captured.

```text
<store>/
├── metadata.sqlite3       SQLite metadata
├── metadata.sqlite3-wal   SQLite-managed while WAL has frames
├── metadata.sqlite3-shm   SQLite-managed coordination file
├── writer.lock            persistent diagnostics; kernel-locked while owned
├── objects/
│   └── ab/
│       └── cdef...        immutable object envelope (64 hex digits total)
├── control/
│   └── active.sock         transient marker endpoint while a run is active
└── runs/
    └── <run-id>/
        └── workspace/      retained isolated run workspace
```

SQLite sidecar presence is not stable API. Temporary object files live in the
target fan-out directory as `.tmp-<pid>-<sequence>` and are removed after
publication. A crash may leave a temporary or unreachable object; neither is a
committed metadata reference. GC reconciles indexed reachability with recognized
digest-shaped unindexed objects and `.tmp-<pid>-<sequence>` files while holding
the writer lock. Store directories are created owner-only; new database, lock,
object, and control-socket files are mode `0600` on supported Unix hosts.

## Identifiers and units

- Run, branch, checkpoint, event, and terminal-stream IDs are canonical
  lowercase UUIDv7 values generated locally. `ProcessId` is a typed nonzero host
  process number, not a durable global identity.
- `ObjectId` and `SnapshotId` are lowercase 64-character BLAKE3 digests.
- Wall-clock fields are signed Unix milliseconds.
- Monotonic duration/offset fields are nonnegative nanoseconds relative to run
  start. They are meaningful only within that run.
- Event sequence is a positive integer unique within a run, starts at `1`, and
  defines order. A stored zero is rejected by typed decoding even if a lower
  schema constraint can represent it.

Numeric conversions reject values that cannot be represented by SQLite's signed
integer storage.

## Object identity and path

For logical bytes `B`:

```text
ObjectId = lowercase_hex(BLAKE3(B))
path     = objects/<hex[0..2]>/<hex[2..64]>
```

The digest covers uncompressed logical bytes, not the storage envelope. Future
compression therefore does not change identity. The current implementation
supports only `none`.

### Version 1 object envelope

All integer fields are little-endian.

| Offset | Bytes | Field | Version 1 value |
| ---: | ---: | --- | --- |
| 0 | 4 | magic | ASCII `RWOB` |
| 4 | 1 | envelope version | `1` |
| 5 | 1 | compression | `0` (`none`) |
| 6 | 2 | reserved | zero; nonzero is rejected |
| 8 | 8 | logical length | unsigned 64-bit byte count |
| 16 | remaining | payload | exactly `logical length` bytes |

Decode rejects bad magic, truncated headers, unknown version/compression,
nonzero reserved bytes, platform-size overflow, and length mismatch. An object
read validates the envelope length against the physical file and the caller's
byte ceiling before allocation. In-memory reads reserve fallibly; streaming
reads hash in bounded memory and verify the length and path-derived BLAKE3 ID at
EOF. Streaming consumers must drain the reader through EOF; bundle export does
so before accepting an entry.

The envelope intentionally does not duplicate the digest. The expected digest
is supplied by its immutable filename/metadata reference and verified by
complete reads or the explicit verification path.

## Object publication

1. Hash the logical bytes and compute their final fan-out path.
2. If the path exists, decode and verify its logical bytes. Equal content is
   idempotent success; disagreement is corruption.
3. Create a unique temporary file in the same directory with `create_new`.
4. Write the complete envelope and `sync_all` the temporary file.
5. Atomically publish without overwriting an existing name.
6. Synchronize the containing directory and remove the temporary name.
7. Record object sizes/compression in SQLite only after publication succeeds.

The current implementation publishes with a same-directory hard link from the
private temporary inode, then immediately removes the temporary name. This is
an atomic no-overwrite installation primitive, not a writable hard-linked
workspace clone. Concurrent installation of the same object is verified and
accepted.

## SQLite configuration

Writable connections configure:

```text
foreign_keys = ON
journal_mode = WAL
synchronous = FULL
busy_timeout = 5 seconds
```

Read-only connections open with SQLite read-only/no-mutex flags and set
`foreign_keys=ON`, `query_only=ON`, and the same bounded busy timeout. They do
not acquire the writer lock.

Both `PRAGMA user_version` and the singleton `schema_metadata.version` must
equal the supported schema version. A newer or mismatched schema fails closed.
Numbered migrations execute in `IMMEDIATE` transactions. Applied migration text
is immutable; changes require a new number plus empty and upgrade tests.

## Schema version 2

Version 1 creates the tables; version 2 adds query indexes. Core tables are
SQLite `STRICT` tables with foreign keys and check constraints.

| Table | Purpose and key invariants |
| --- | --- |
| `schema_metadata` | singleton applied version and update time |
| `objects` | digest, logical/stored sizes, compression, creation time |
| `snapshots` | snapshot digest, entry count, logical bytes |
| `snapshot_entries` | one typed normalized path per snapshot; file/object, directory, and symlink shapes are mutually constrained |
| `runs` | branch, command, workspace, platform, capture policy, status, snapshots, duration and exit code |
| `run_arguments` | ordered command arguments without lossy joining |
| `run_parents` | at most one parent run/checkpoint edge; self-parent forbidden and cycle checked by repository operation |
| `events` | `(run_id, sequence)` primary key plus unique event ID, times, payload schema version/kind, and valid JSON payload |
| `checkpoints` | unique run sequence, reason, snapshot, label, and times |
| `terminal_chunks` | stream range mapped to an immutable object and byte length |
| `warnings` | run/sequence-keyed diagnostic code and message |

Event payload JSON is not unversioned: `kind` and `schema_version` select the
typed durable payload decoder. Unknown required versions/variants fail with a
structured corruption/compatibility error. Large terminal/file bytes never
belong in payload JSON.

Indexes serve current list/timeline/ancestry/reachability queries: run start,
event wall time with sequence tie-break, checkpoint snapshot, parent run,
snapshot object, and terminal sequence range.

## Snapshot canonical form

A snapshot entry contains a normalized relative UTF-8 path, entry kind, file
object ID or symlink target as appropriate, executable bit, supported Unix mode,
and logical size. Manifest schema version 1 sorts entries by normalized path and
serializes the fixed-field domain structs with compact `serde_json::to_vec`.
The result is deterministic canonical JSON for this schema; it is not a claim
that arbitrary JSON documents are normalized. Identity is:

```text
SnapshotId = lowercase_hex(BLAKE3(canonical_manifest_json))
```

SQLite row order, timestamps, inode numbers, and absolute source paths are not
part of the identity. Loading a snapshot reconstructs entries in path order,
re-encodes the manifest, and rejects a digest, entry-count, logical-size, or
schema mismatch.

Workspace scans pin the root and open each regular-file component
descriptor-relative without following symlinks; clone traversal and retained
workspace exclusion cleanup use the same beneath-root rule. Materialization
preflights the complete tree for absolute/traversal/prefix paths,
duplicate normalized paths, case-fold collisions on the destination, invalid
kind fields, and missing/corrupt objects before writing. Symlinks are recreated
without being followed. Restore pins the destination parent and private sibling
tree with directory descriptors, performs descendant writes, cleanup, backup,
and installation relative to those descriptors, then renames the tree into
place. Replacing a nonempty destination requires an explicit force option.
Supported file and directory modes are restored; restrictive directory modes
are flushed through descriptors opened before chmod. Symlink mode bits are
reported when they cannot be applied safely without following the link.

## Transaction and crash boundaries

- Object publication precedes any SQLite reference to the object.
- `commit_checkpoint_with_event`, used by capture, writes snapshot metadata,
  entries, checkpoint, and its exactly matching committed event in one
  transaction after required objects are durable. The lower-level
  `commit_checkpoint` operation omits the event intentionally.
- `append_event_batch` validates strictly increasing contiguous run order at the
  repository boundary.
- `finish_run_with_events` appends the batch ending in a matching
  `RunCompleted` event and stores terminal state/final-snapshot metadata in one
  transaction; a terminal run cannot transition again. `finish_run` remains
  available for recovery transitions that do not invent a completion event.
- Startup recovery changes abandoned `Preparing`/`Running` runs to
  `Interrupted` without fabricating a final checkpoint.

Repository tests exercise these transactions, reject a discontinuous batch
without partial insertion, and reopen an unfinished run through startup
recovery. GC validates typed terminal payload reachability against denormalized
event kinds before deletion, and tests create/remove both an unindexed published
object and a temporary publication artifact. Tests do not yet inject process death or I/O
faults at every listed boundary, so crash behavior beyond SQLite/object-store
guarantees remains a hardening target.

## Writer lock

Writable access opens or creates `writer.lock` without following a final
symlink, verifies that it is a regular file, and acquires a nonblocking exclusive
kernel advisory lock on its open descriptor. The file remains after release and
contains version 1 diagnostics bounded to 16 KiB:

```text
version=1
pid=<decimal>
created_unix_ms=<decimal>
token=<unique owner token>
```

PID, timestamp, and token are diagnostics, not ownership authority. Dropping the
guard or terminating the process closes the descriptor and releases ownership;
the next writer can reuse the persistent file without manual deletion. Lock
inspection probes the kernel lock, so an inactive diagnostics file is not
reported as a current owner. The lock is cooperative between Rewind writers;
read-only inspection remains possible while it is held.

## Version 1 bundle framing

RWBN version 1 is a deterministic checksummed sequence of named byte payloads.
It is the framing used by `rewind export --format bundle`; it is not a copy of
the store directory. All integer fields are little-endian.

### File header

| Offset | Bytes | Field | Version 1 value |
| ---: | ---: | --- | --- |
| 0 | 4 | magic | ASCII `RWBN` |
| 4 | 2 | version | `1` |
| 6 | 2 | flags | zero; nonzero is rejected |
| 8 | 4 | entry count | unsigned number of following entries |

### Repeated entry

Offsets below are relative to the start of each entry.

| Offset | Bytes | Field | Version 1 value |
| ---: | ---: | --- | --- |
| 0 | 2 | path length | unsigned UTF-8 byte count |
| 2 | 2 | flags | zero; nonzero is rejected |
| 4 | 8 | payload length | unsigned byte count |
| 12 | 32 | payload checksum | raw BLAKE3 digest of the payload |
| 44 | `path length` | path | normalized relative UTF-8 path |
| following | `payload length` | payload | exact entry bytes |

Entry paths are strictly increasing and unique. Empty, absolute, traversing,
backslash-containing, NUL-containing, empty-component, and paths longer than
4096 bytes are rejected. Decode also rejects truncation, trailing bytes,
unsupported versions or flags, checksum mismatch, host numeric overflow, and
caller-provided entry/per-entry/aggregate size-limit violations.

The current exporter writes these logical entries in path order:

- `manifest.json`: bundle version, run ID, event encoding, redaction report,
  and logical-entry checksums;
- `run.json`: typed export metadata with the local absolute workspace path
  omitted; `checkpoints.json`: typed checkpoint JSON;
- `events.ndjson`: one typed event JSON record per authoritative sequence;
- `snapshots/<snapshot-id>.json`: referenced snapshot records;
- `objects/<first-two-hex>/<remaining-hex>`: reachable logical object bytes,
  without their local RWOB envelopes.

Each RWBN entry, including `manifest.json`, has the checksum embedded in its
frame. The manifest additionally lists checksums and lengths for the other
logical entries. Export can replace recorded terminal-input events with
`TerminalInputRedacted` records and records that choice in the redaction report.
Process executables beneath the isolated run root are emitted relative to
`<workspace>`, and diagnostics replace that same absolute root.
The completed file is written through a temporary sibling, flushed, renamed,
and followed by a parent-directory flush.

The codec has bounded decode APIs and unit/fuzz targets, but there is no bundle
import or extraction command. Decoding an RWBN file alone does not validate its
domain graph or install it into a store. No database or object directory should
be presented as a portable bundle by simply archiving the store root.
