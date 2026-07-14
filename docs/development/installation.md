# Local installation

Rewind has no published package. Build it from this checkout with the configured
stable Rust toolchain:

```bash
cargo build --release --locked --bin rewind
install -m 0755 target/release/rewind "$HOME/.local/bin/rewind"
```

Generate shell completions and a manual page from the live command definition:

```bash
cargo xtask package
ls target/completions target/man/rewind.1
```

The binary stores data under the platform application-data directory. For an
isolated or removable store, set `REWIND_HOME` before every command:

```bash
export REWIND_HOME="$PWD/.local-rewind-data"
rewind doctor
```

Do not place `REWIND_HOME` beneath a workspace that will be captured; Rewind
rejects that recursive layout.

Supported first-release targets are macOS Apple Silicon and modern Linux on
x86-64 or ARM64. There is no Windows build in this release line.
