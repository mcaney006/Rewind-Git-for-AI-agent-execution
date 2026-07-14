# Release checklist

Releases and publication are performed only by the repository owner. These
steps prepare local evidence; none creates a commit, tag, account, upload, or
package publication.

1. Run `cargo xtask check` and `cargo xtask test-all`.
2. Compile fuzz targets with an explicitly installed `cargo-fuzz` and nightly
   toolchain; record tool versions and the duration of any fuzz campaign.
3. Run `cargo xtask demo` from a clean `REWIND_DEMO_DIR` and inspect both
   replays, the checkout, fork ancestry, and comparison evaluation.
4. Run the benchmark harness on named hardware and record OS, architecture,
   filesystem, Rust version, fixture variables, samples, and raw output.
5. Run `cargo xtask package`; inspect the release binary, completions, and man
   page under `target/`.
6. Verify an orderly SIGINT capture, startup recovery diagnostics, bundle
   decoding limits, HTML escaping, and GC dry-run/delete behavior.
7. Search for unfinished implementation markers and prohibited attribution;
   inspect `git diff`, `git status`, and the existing commit graph.
8. Update `CHANGELOG.md` with only verified behavior and known limitations.
9. The owner may then decide whether and how to commit, tag, sign, or publish.

Do not call the result production-ready solely because this checklist passes.
