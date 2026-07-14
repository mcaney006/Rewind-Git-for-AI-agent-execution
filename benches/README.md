# Benchmark methodology

Run the dependency-free harness with:

```bash
cargo xtask bench
```

The equivalent direct invocation is
`cargo bench -p rewind-benchmarks --features run-benchmarks`.

It prints measurements from the current machine; this repository does not
publish baseline performance claims. Record the CPU, memory, operating system,
filesystem, Rust version, power mode, and whether the volume supports clones
when comparing runs.

The default workloads are:

- authoritative initial scan and CAS insertion for 2,000 distinct small files;
- authoritative rescan after 32 files change, including a deduplication check;
- transactional checkpoint metadata commit for a 2,000-entry snapshot;
- verified atomic materialization of that snapshot;
- comparison evidence loading and deterministic diff for two completed runs;
- transactional ingestion of 10,000 terminal frames in 256-event batches and
  bounded 256-event timeline queries.

Set `REWIND_BENCH_SAMPLES`, `REWIND_BENCH_FILES`,
`REWIND_BENCH_CHANGED_FILES`, or `REWIND_BENCH_EVENTS` to positive integers to
adjust the workload. Fixture construction and initial setup are outside the
reported intervals. Temporary data is removed after each sample.
