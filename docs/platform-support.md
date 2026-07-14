# Platform support

Rewind targets macOS on Apple Silicon and modern Linux on x86-64 and ARM64.
Windows and other targets are outside the first release and must fail clearly
rather than take an untested fallback path.

> **Verification status (2026-07-13):** the development host is macOS 26.6,
> Apple Silicon, on a case-insensitive APFS volume. Rust/Cargo 1.96.1,
> `fclonefileat(2)`, PTYs, `libproc`, SQLite, the full workspace test suite, a real
> generic capture, and the breaker/fixer demo have run locally. Linux code and
> CI jobs are present, but no Linux workflow result was observed in this work.

## Support matrix

| Target | Design status | Verification |
| --- | --- | --- |
| `aarch64-apple-darwin` | first class | locally verified on macOS 26.6/APFS |
| `x86_64-unknown-linux-gnu` | first class | implemented and CI configured; runtime result pending |
| `aarch64-unknown-linux-gnu` | first class | implemented and CI configured; runtime result pending |
| Windows | unsupported initially | startup/compile failure required |
| Other Unix targets | unsupported initially | no compatibility claim |

“First class” defines the intended support obligation, not proof that every
feature currently passes on that target.

## Platform boundary

Target-specific conditionals and native behavior live in `rewind-platform`, not
domain, store, or snapshot policy. Narrow portable `cfg(unix)` metadata guards
remain local where no OS-specific behavior is selected. Safe callers see narrow
operations and explicit capability results for:

- per-file copy-on-write cloning;
- PTY allocation and terminal-size propagation;
- supervised process inspection;
- advertised watcher capability and portable dirty-path polling;
- disk-space and filesystem diagnostics.

A capability probe distinguishes unsupported behavior from transient failure.
For example, an unsupported reflink selects recursive copy, while permission or
I/O failure is returned with context instead of silently falling back.

Unsafe code is denied at workspace level. A platform module may opt into the
minimum required unsafe scope only with a `SAFETY` explanation, an owned safe
result, and focused wrapper tests.

## Workspace clone strategies

The selected strategy is stored as a typed run event and shown by the capture
summary.

### macOS

Rewind opens the source tree descriptor-relative without following symlink
components and attempts `fclonefileat(2)` from each pinned regular-file
descriptor on clone-capable APFS volumes. Destination directories are created
explicitly. Symlinks are recreated from descriptor-relative `readlinkat` data
and are not followed.

### Linux

Rewind opens every source component with `openat` and `O_NOFOLLOW`, verifies
descriptor metadata, and attempts `ioctl(FICLONE)` from the pinned source file
on filesystems that support reflinks. The call may
legitimately report unsupported across filesystems or on some mounts;
capability is not inferred from the OS name alone.

### Portable fallback

If copy-on-write cloning is unsupported, Rewind recursively copies bytes and
supported metadata. It preserves directory structure, symlinks, Unix mode, and
executable bits. The fallback uses the same pinned source descriptors and never
uses writable hard links as copy-on-write substitutes. A run records the
fallback and warns that large workspaces may be expensive.

Clone acceleration does not define snapshot correctness. The initial and final
authoritative scans do.

## Filesystem behavior

- Snapshot paths are normalized relative UTF-8 paths. Non-UTF-8 names are
  rejected explicitly in the initial format.
- Stored symlinks are never followed during scan or materialization.
- Materialization rejects absolute paths, `..`, platform prefixes, NULs, and
  paths that escape the destination.
- All destination paths are preflighted. Case-folding collisions are rejected
  on case-insensitive destinations.
- A normal `.git` directory is preserved so generic Git commands work. A
  top-level `.git` indirection file is rejected initially because it can point
  to shared worktree metadata outside the isolated directory.
- `REWIND_HOME` is resolved before capture and must not be recursively included
  in the source workspace.

Extended attributes, ACLs, flags, ownership, sparse extents, and platform birth
times are not part of the initial portable snapshot identity unless later added
as versioned metadata. Rewind reports this limitation rather than claiming a
byte-for-byte filesystem image.

## PTY and terminal behavior

The supervised command runs under a real pseudoterminal. Platform wrappers must
support raw byte forwarding, window-size get/set, echo-state observation,
signal forwarding, child reaping, and RAII restoration of the parent terminal.

Echo-state observation is inherently best effort and can race the child. It is
not a password detector. Sensitive sessions should use
`--record-input=never`.

## Process observation

- macOS uses a narrow `libproc` wrapper where available.
- Linux transiently enumerates numeric `/proc` entries to build parent links,
  then filters to the supervised root and its discovered descendants before
  returning or persisting observations. It does not retain unrelated process
  metadata.
- Bounded polling is acceptable; process records are observational, not an
  authoritative audit trail. Very short-lived descendants can be missed.

Rewind does not persist unrelated processes and is not endpoint detection
software.

## Watchers

The current recorder uses a descriptor-rooted metadata poller for dirty-path
hints. Pending paths are configurable; total hint enumeration is capped at one
million entries and disables itself visibly if exceeded. Native macOS/Linux
watcher primitives are reported by capability
diagnostics but are not wired into capture yet. Any future watcher may coalesce,
reorder, or lose events, so checkpoint creation will still read actual paths and
finalization will still perform a complete scan. Watchers are an optimization,
not the source of snapshot truth.
