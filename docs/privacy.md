# Privacy

Rewind observes terminal sessions and source trees, which routinely contain
credentials and proprietary code. Privacy is a capture-time constraint, not an
export checkbox applied after everything has been stored.

> **Status:** the capture and export defaults below are implemented, including
> terminal-input redaction and orderly exclusion cleanup. Rewind is still not a
> secrets boundary; review stored data before sensitive use or sharing.

## Default behavior

- Storage is local. There is no telemetry, analytics, cloud sync, crash upload,
  or background network service.
- Rewind captures only the configured workspace plus metadata about the
  supervised process tree.
- Environment variables are not captured. Enabling environment capture is
  currently rejected rather than treated as a no-op.
- Terminal input uses `auto`: bytes entered while the child terminal reports
  echo disabled are replaced with a redacted input event.
- Terminal output is recorded as emitted. Output can contain secrets; Rewind
  cannot reliably classify or undo them.
- Workspace file content is recorded unless a visible ignore/exclusion or size
  policy applies. No build or dependency-cache directory is ignored by default,
  because silently omitting it can change command behavior.
- Rewind does not access keychains, credential stores, or private agent data.
- Debug and tracing output never includes terminal input or file contents.

## Terminal input

`--record-input` has three policies:

| Value | Behavior |
| --- | --- |
| `auto` | store bytes only while terminal echo appears enabled; otherwise store timing and byte length in `TerminalInputRedacted` |
| `always` | store all forwarded input bytes; intended only with informed consent |
| `never` | store no input bytes; store redacted timing/length events |

`auto` is best effort. The input producer samples a duplicated PTY descriptor
immediately after each stdin read and before forwarding those bytes, then sends
that decision with the observation so recorder backpressure cannot delay the
classification. Echo can still race the read itself, and programs can collect
sensitive text with echo enabled. Use `never` for password, token, production,
or otherwise sensitive interactive work.

Redaction preserves byte length to support timing playback. That length is
metadata and can itself reveal limited information; a future stricter policy may
coarsen it, but the initial format must not pretend it is content-free.

## Workspace exclusions

Built-in privacy defaults recognize common high-risk secret filenames at both
the workspace root and nested paths. They are a conservative convenience, not
complete secret detection. Project and user configuration can add excluded
paths, ignored paths, and file-size limits. Binary classification is not yet
implemented; `binary_files = "exclude"` is rejected rather than ignored.

When policy excludes a file:

1. its content is not stored;
2. the run records that an exclusion affected replay fidelity;
3. normal local inspection may identify the path when needed to diagnose the
   workspace;
4. export uses a redaction token or count instead of leaking the path unless the
   export policy explicitly permits it;
5. after the child stops and the final authoritative snapshot commits, orderly
   finalization removes the union of excluded relative paths from the retained
   run workspace through pinned directory descriptors without following any
   symlink component.

Removing a path is not secure erasure: copy-on-write filesystems, SSDs, backups,
and filesystem journals may retain old blocks. A recorder crash or power loss
between workspace cloning and orderly cleanup can also leave excluded bytes in
the private run workspace. Startup recovery can identify the interrupted run,
but cannot reconstruct exclusion paths that were never persisted. Treat an
interrupted run directory as sensitive and remove it before sharing the store.

Ignore and privacy exclusion are distinct. Ignore says a path is outside the
versioned workspace model; exclusion says sensitive content was deliberately
withheld and replay is incomplete.

## Environment

Environment values are not captured in this version. Setting
`capture_environment = true` is rejected rather than accepted as a no-op; the
allowlist and denylist fields are reserved for a future versioned record.
Rewind cannot prevent the child from printing an environment value into its
terminal or writing it into a captured file.

The child still receives the environment selected by normal command execution.
“Not captured” does not mean “not available to the child.” Rewind is not a
sandbox.

## Configuration precedence

One strict TOML format is used with this precedence:

```text
CLI flags > project .rewind.toml > user configuration > built-in defaults
```

Unknown fields are rejected rather than silently ignored. Project configuration
is read only at the documented project boundary; Rewind does not walk arbitrary
ancestors. The run record stores the input-recording policy and whether
environment capture was enabled (always false in this version). Generic
warnings and counts make exclusions visible, but resolved path patterns,
file/terminal limits, and checkpoint timing settings are not yet persisted as a
complete durable policy.

Per-file, terminal-output, and per-run unique logical-object limits are
enforced. The run limit is not a physical disk quota: rejected scans can leave
unreachable objects, and retained workspaces and envelopes occupy additional
bytes. See [Configuration](configuration.md) for exact field support.

## Local storage and access

Large run data lives under the platform application-data directory or the
explicit `REWIND_HOME`, not inside `.git`. Rewind creates control sockets,
temporary files, and locks in permission-restricted directories. The SQLite
database and object store should be readable only according to the owning
user's configured filesystem permissions.

Local storage is not encryption at rest. Anyone with access to the user's files
may read captured source and terminal data. Disk encryption and host access
control remain the user's responsibility.

## Export

Export is an explicit operation. Bundle and HTML export:

- apply the selected terminal-input export redaction policy;
- include a terminal-input redaction report;
- include no telemetry or remote assets;
- escape terminal and file text before HTML insertion;
- enforce explicit artifact-size ceilings;
- avoid paths or environment data not required for replay.

Excluded path names and content are absent from snapshot exports, but there is
currently no content-scanning redactor. The local absolute run-workspace path is
omitted from both formats. Bundle process executables beneath that workspace are
rendered relative to `<workspace>`, and recorder/failure diagnostics replace the
same absolute root. An export cannot reliably remove a secret that
appeared in arbitrary terminal output, command arguments, or an ordinary
included file. Review artifacts before sharing them.

## Deletion and retention

Deleting run metadata does not make an immutable object unreachable if another
run or snapshot references it. Garbage collection is reachability-based and is
dry-run by default. Secure erasure is not guaranteed on copy-on-write or
flash-backed filesystems; filesystem and device behavior govern physical data
removal.

## Privacy limitations

Rewind cannot guarantee detection of passwords, tokens, personal data, or
secrets. It cannot retract data copied to external systems by the child process,
and it does not prevent a malicious child from reading anything the operating
system account already permits. See the [threat model](threat-model.md).
