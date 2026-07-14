# Threat model

Rewind records code, files, commands, and terminal data under a developer's
account. This model protects store integrity, the source workspace, restore
destinations, and sensitive recorded data from malformed or hostile inputs.

Rewind is **not a security sandbox**. A supervised command has the operating
system permissions of the user who launched it and can cause network or other
external side effects that Rewind cannot undo.

> **Status:** implemented controls and remaining gaps are named below. Bounded
> decoders and object reads, descriptor-rooted path operations, migrations,
> advisory locking, HTML escaping, PTY shutdown, and orderly exclusion cleanup
> have focused tests. Systematic process-kill fault injection and bundle graph
> import are not implemented.

## Assets

- the user's original source workspace and unrelated filesystem data;
- integrity and availability of run metadata, snapshots, and object bytes;
- terminal input, output, captured file content, and environment values a child
  may expose through those recorded surfaces;
- correctness of ancestry, event ordering, and checkpoint references;
- terminal state and process cleanup after recorder exit;
- safe replay/export on a user's machine or in a browser.

## Trust boundaries

Untrusted inputs include the child process, workspace names and bytes, symlink
targets, terminal streams, control-socket frames, bundle bytes, on-disk
database/object corruption, configuration, and destination paths. SQLite,
filesystem, PTY, and platform API failures are expected operational faults, not
invariant exceptions.

The local user and operating system kernel are trusted to enforce account and
file permissions. Third-party commands, repositories, bundles, and terminal
recordings are not trusted. A compromised account or kernel is out of scope.

## Threats and controls

### Malicious child process

**Threat.** The child writes outside the run workspace, consumes resources,
spoofs terminal content, keeps descendants alive, connects to remote systems,
or attacks the recorder through its PTY/control surfaces.

**Controls.** Default execution uses an isolated workspace; capture boundaries
and frame sizes are bounded; only the supervised tree is observed; signals and
reaping use structured shutdown; recorder metadata is stored outside captured
content; control endpoints are not inherited unnecessarily. After the root
exits, Rewind checks and terminates its owned process group before the final scan
even when PTY output has already closed. PTY draining has a grace period and one
final bounded drain before a durable incomplete-tail warning.

**Residual risk.** Filesystem isolation is not process sandboxing. The child can
access anything the launching user can access and can leave external side
effects. A descendant that escapes the supervised process group while retaining
the PTY can outlive capture; Rewind stops waiting and may detach its blocked
reader after warning, so the terminal tail can be incomplete. Stronger OS
sandboxing would be a separate opt-in feature.

### Malicious workspace paths

**Threat.** Absolute paths, `..`, prefixes, NULs, ambiguous encodings, or
case-fold aliases escape or overwrite a restore destination.

**Controls.** Snapshot paths are normalized relative UTF-8 components. Scan and
decode reject invalid paths. Restore validates every path and destination
collision before writing, pins the destination parent/private sibling with
directory descriptors, and performs writes, rollback, cleanup, and installation
relative to those descriptors. Property tests exercise traversal, restrictive
directory modes, parent replacement, and normalization.

**Residual risk.** A same-account attacker can rename the requested parent path;
descriptor pinning keeps writes in the opened directory but cannot make the
human-readable path namespace immutable.

### Symlink attacks

**Threat.** A workspace or imported tree uses a symlink to read outside source
content during scan or redirect restore writes.

**Controls.** Scans use link metadata and record the link target without
following it. Regular-file bytes are opened from the workspace descriptor one
component at a time with no-follow semantics, so a symlink ancestor cannot
redirect content capture. Exclusion cleanup uses the same descriptor-relative
rule for every component. Initial clone traversal likewise reads directories,
files, and symlink targets through pinned descriptors. Restore creates links
only after path preflight and never writes through a restored link. A top-level
`.git` indirection file is rejected for isolated runs. Destination creation
refuses hostile pre-existing content unless an explicit destructive path safely
replaces the entire tree.

**Residual risk.** A concurrently mutating source can race scan. Rewind reports
that checkpoint scans and workspace clones are observational rather than
filesystem-atomic; descriptor pinning prevents symlink redirection but does not
turn a changing directory tree into one atomic point-in-time view.

### Archive traversal and untrusted bundles

**Threat.** Bundle entries escape extraction, duplicate normalized paths,
declare excessive sizes, reference missing objects, or provide forged hashes.

**Controls.** The RWBN decoder is versioned, size-limited, and strict; unknown
mandatory versions fail closed. Every entry path is normalized, ordered, and
unique. Declared and actual counts/sizes are bounded, and each payload checksum
is verified. Fuzz targets cover bundle framing and path validation.

**Residual risk.** Rewind has no bundle import command. The framing decoder does
not validate a complete run/snapshot/object graph, so decoded bundles must not
be installed into a store. A future importer needs a separate private staging
and transactional graph-validation path.

### Oversized or malformed events

**Threat.** A child or control client sends an oversized frame, creates
unbounded queue growth, or triggers pathological parser allocation.

**Controls.** Length-prefix decoders validate the prefix before allocation;
event, control-frame, terminal-chunk, and per-file limits are explicit;
producer channels are bounded; persistence failure applies backpressure or
terminates capture visibly. RWOB readers compare declared and physical sizes
with a caller ceiling before allocation, reserve fallibly, and provide bounded
streaming verification. Parsers are fuzzable and do not recurse without a depth
bound.

**Residual risk.** Blocking backpressure can make the child appear stalled. The
recorder reports the cause instead of silently dropping events. The unique
logical-object quota is checked before event/checkpoint references, but a
snapshot may already have published objects that become unreachable when its
checkpoint is rejected; GC, not the per-run quota, reclaims those objects.

### Malformed database content

**Threat.** Corruption or manual edits create invalid enum values, broken
foreign keys, impossible state transitions, cycles, or nonsensical sizes.

**Controls.** SQLite foreign keys, uniqueness/check constraints, transactional
migrations, and typed decoding reject invalid core records. Read APIs validate
durable invariants and return contextual corruption errors. Parent cycles are
checked at the persistence boundary. Migration failure leaves the prior schema
intact.

**Residual risk.** SQLite cannot encode every domain invariant. `doctor` and
load paths must not trust rows merely because a query returned them.

### Corrupted or substituted objects

**Threat.** Object bytes no longer match their ID, an import lies about a
digest, or concurrent writers replace content.

**Controls.** Object identity is BLAKE3 of logical uncompressed bytes. In-memory
reads enforce caller ceilings; streaming consumers verify at EOF. The
object-import primitive verifies hashes, but no bundle import command exists.
Writes use private temporary files, flush, and atomic non-overwriting
publication. Concurrent publication of identical bytes is idempotent; different
bytes at an existing ID are corruption. Checkpoint metadata references only
durable objects.

**Residual risk.** BLAKE3 protects integrity, not confidentiality or malicious
deletion. Backups and filesystem permissions remain external responsibilities.

### Terminal escape sequences

**Threat.** Recorded output changes terminal state, injects clipboard/control
sequences, or becomes HTML/script injection in export.

**Controls.** Terminal bytes remain data. Native replay uses a constrained ANSI
parser/emulator and must not blindly write dangerous control sequences to the
operator terminal. HTML export emits escaped text and a fixed allowlist of
formatting; no raw HTML is accepted. OSC hyperlinks and clipboard operations
are disabled or rendered inert. Exact raw replay bytes are written only when
stdout is redirected; a mixed noninteractive-input/live-terminal path fails.

**Residual risk.** ANSI emulation is complex and visual spoofing remains
possible. Replay identifies recorded content but does not provide a separate
user-selectable escaped-text view.

### Sensitive terminal input and captured secrets

**Threat.** Passwords, tokens, environment values, or secret files enter the
local store or a shared export.

**Controls.** Environment capture is off and attempts to enable it are rejected;
input policy defaults to best-effort echo-aware redaction; `never` records no
bytes; known sensitive paths have
default exclusions; logs exclude input and file bytes; exports apply explicit
redaction and include a report. After the supervised child stops, orderly
finalization unlinks excluded files and symlinks and recursively removes
excluded directories from the retained run workspace without following any
symlink path component.

**Residual risk.** Echo detection and secret-name heuristics are not infallible,
and output or ordinary files can contain secrets. Deletion is not secure erasure
on copy-on-write storage, SSDs, journals, or backups. A crash before orderly
cleanup can leave excluded bytes in the private run workspace because exclusion
paths are intentionally absent from durable metadata. Users should select
`never`, treat interrupted run directories as sensitive, and review exports.

### Concurrent recorder instances

**Threat.** Writers interleave migrations, events, checkpoints, garbage
collection, or diagnostic updates and create dangling references.

**Controls.** A store-wide nonblocking kernel advisory lock is held through a
no-follow regular-file descriptor. Its persistent file carries bounded owner
diagnostics, but kernel ownership—not its PID text—decides contention. Descriptor
close or process termination releases the lock automatically. Per-run ownership
prevents a second recorder finalizing the same run. SQLite WAL supports readers;
foreign keys and transactions commit multi-row changes. The terminal event batch
and run-state transition share one transaction. GC requires exclusive mutation
ownership, validates typed terminal references before deletion, and protects
active runs.

**Residual risk.** The lock is advisory and assumes cooperating Rewind writers.
A malicious process running as the same account can ignore the lock or replace
store paths; permission isolation does not defend against a compromised account.

### Interrupted writes and recorder crashes

**Threat.** Power loss or termination leaves partial objects, databases,
workspaces, or terminal state.

**Controls.** Files are written to private temporary paths, flushed, then
atomically renamed; directory metadata is synchronized where supported and
necessary. Metadata commits are transactional and occur after referenced
objects are durable. Kernel lock release lets the next writer run startup
recovery, which marks abandoned preparing/running records interrupted. Under
that lock, GC reconciles indexed reachability with recognized unindexed fan-out
objects and `.tmp-<pid>-<sequence>` publication artifacts. Controlled exits
explicitly restore terminal state and surface failure; an RAII guard remains the
panic fallback. The child and its owned process group are reaped/terminated
before a final checkpoint is trusted.

**Residual risk.** Sudden power loss can prevent process cleanup or terminal
restoration and can leave unreachable objects. Recovery diagnoses rather than
inventing missing history.

### Local control socket

**Threat.** Another local process creates false markers, sends malformed frames,
or binds a predictable endpoint.

**Controls.** The Unix-domain socket and parent directory are mode restricted,
the protocol is versioned and length limited, responses identify the active
run, malformed data is rejected, and no network interface is used. Decoder
fuzzing covers arbitrary bytes.

**Residual risk.** Processes running as the same account may share filesystem
authority. Peer credentials are not separately authenticated; filesystem modes
are the boundary. The control socket is a local coordination channel, not a
strong authentication boundary.

## Security validation status

Focused tests currently cover path normalization and symlink-ancestor swaps;
bounded object envelopes, corruption, and idempotent non-overwriting
publication; empty and upgrade migrations; HTML script-data escaping; kernel
lock contention and release; exclusion cleanup; physical GC artifacts; and
bounded post-root PTY shutdown. Decoder fuzz targets exist for control frames,
object envelopes, bundle framing, archive paths, and typed event JSON, but no
local fuzz campaign has been claimed.

Still required before release are systematic process-kill and I/O-fault tests
across object/checkpoint/finalization boundaries, broader hostile terminal/ANSI
fixtures, and transactional graph validation if bundle import is added.
