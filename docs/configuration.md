# Configuration

Rewind uses one strict TOML format. Unknown sections, fields, enum values,
units, and misspellings are errors rather than silently ignored settings.

## Locations and precedence

For capture, values resolve in this order:

```text
`--record-input` CLI flag
project `<workspace>/.rewind.toml`
user configuration
built-in defaults
```

The project file is read only from the selected workspace root; Rewind does not
walk parent directories. The user file is:

- macOS: `~/Library/Application Support/Rewind/config.toml`;
- Linux: `${XDG_CONFIG_HOME:-~/.config}/rewind/config.toml`;
- with `REWIND_HOME`: `<REWIND_HOME>/config.toml`.

`REWIND_HOME` changes the store and, by design, its user-configuration location
for that invocation. The path must not be inside a workspace being recorded.
Each configuration file is limited to 1 MiB and opened beneath a pinned parent
directory without following symlink components.

## Complete initial format

```toml
[workspace]
ignore = []
max_file_size = "64 MiB"
follow_symlinks = false
binary_files = "record"

[capture]
record_input = "auto"
checkpoint_debounce = "750 ms"
checkpoint_min_interval = "2 s"
checkpoint_max_interval = "60 s"
maximum_pending_dirty_paths = 10000
process_poll_interval = "250 ms"
terminal_chunk_size = "64 KiB"
terminal_max_bytes = "2 GiB"

[storage]
max_run_size = "10 GiB"
compression = "never"

[privacy]
capture_environment = false
environment_allowlist = []
environment_denylist = ["*TOKEN*", "*SECRET*", "*PASSWORD*"]
excluded_paths = [
  ".env", "**/.env", ".env.*", "**/.env.*",
  "*.pem", "**/*.pem", "*.key", "**/*.key",
  "id_rsa", "**/id_rsa", "id_ed25519", "**/id_ed25519",
]
redact_exports = true

[replay]
terminal_cache = "64 MiB"
```

Sizes require a positive integer, one space, and one of `B`, `KiB`, `MiB`, or
`GiB`. Durations require a positive integer, one space, and `ms`, `s`, or
`min`. Decimal quantities and aliases such as `MB` are rejected.

Ignore/exclusion patterns are workspace-root-relative. `*` does not cross `/`;
`**` can. No build or dependency directory is ignored implicitly. Symlinks are
always recorded without following them, so `follow_symlinks = true` is rejected.

## Implemented limits and reserved settings

- `max_file_size`, terminal chunk/output limits, checkpoint timing, dirty-path
  count, process polling, input policy, and replay cache are enforced.
- `redact_exports = true` replaces recorded terminal-input events in bundles
  and omits their objects. It cannot discover secrets in terminal output or
  ordinary files.
- RWOB version 1 is deliberately uncompressed; `compression` accepts only
  `"never"`. Compression will not be advertised before workloads justify it.
- `binary_files = "exclude"` is rejected. Use explicit path patterns until a
  tested binary classifier exists.
- Environment values are never captured in this version.
  `capture_environment = true` is rejected; allow/deny lists are reserved for a
  future versioned environment record rather than being applied invisibly.
- `max_run_size` bounds both bundle export and the unique logical file,
  terminal-output, and retained-input objects referenced by one capture. The
  same `ObjectId` is counted once across checkpoints and streams.

Workspace `ignore`, privacy `excluded_paths`, and oversize-file decisions all
produce visible exclusion warnings and omit content from snapshots. The current
scanner uses one exclusion path pipeline for them and removes their union from
the retained run workspace during orderly finalization. It does not persist
excluded path names into exported artifacts.

## Limitations

The run budget is a logical reference limit, not a physical store quota. A
snapshot must publish immutable objects before its complete unique-object cost
is known; when the subsequent budget check rejects the checkpoint, those
unreferenced objects remain eligible for garbage collection. `rewind gc`
reconciles both indexed reachability and recognized unindexed/temp physical
artifacts. Cross-run deduplication, object envelopes, and retained workspaces
still mean physical disk usage is not equal to `max_run_size`.

Excluded bytes can remain in a private isolated workspace after a crash before
orderly cleanup, and unlinking is not secure erasure on copy-on-write or flash
storage. See [Privacy](privacy.md) and the [threat model](threat-model.md).
