# Rewind fuzz targets

The targets exercise bounded untrusted-input parsers:

```bash
cargo install cargo-fuzz
cargo fuzz run control_frame
cargo fuzz run bundle_decode
cargo fuzz run archive_path
cargo fuzz run event_json
cargo fuzz run object_envelope
```

The bundle target supplies small decode ceilings so a crafted length field
cannot turn one test case into an unbounded allocation. The archive-path,
event, and object-envelope targets similarly cap work per case. Corpus and
crash artifacts remain local and are ignored by Git.
