# ADR 0002: Assign event order in one bounded recorder

- **Status:** Accepted
- **Date:** 2026-07-13
- **Implementation:** Implemented; unit tests verify contiguous sequencing and
  blocking bounded-channel backpressure, and real capture exercises ordered
  terminal/checkpoint persistence. Recorder crash-fault injection remains
  pending.

## Context

PTY output, input, process observations, filesystem hints, signals, and control
messages occur concurrently. Independent database writers would make ordering
dependent on scheduling and complicate checkpoint/event consistency. Unbounded
queues would turn a noisy terminal into unbounded memory growth.

## Decision

Typed producers send observations through bounded channels to one recorder
coordinator. The coordinator alone:

1. assigns strictly increasing event sequences;
2. validates run state;
3. batches event and terminal-object persistence;
4. schedules non-overlapping checkpoint commits;
5. orders finalization.

Sequence, not timestamp, is authoritative. Wall-clock time and monotonic offset
are stored for presentation.

The first implementation uses synchronous threads and bounded
`std::sync::mpsc::sync_channel` channels. When a safe producer reaches capacity,
it blocks. Persistence failure ends capture visibly; no event is silently
dropped. Terminal bytes are persisted in bounded chunks rather than accumulated
for the run.

## Consequences

- One place enforces ordering and run/checkpoint state invariants.
- Backpressure can stall the child when durable storage cannot keep up; this is
  preferable to a false complete recording.
- Synchronous snapshot work can apply bounded backpressure to the child; a
  future concurrent scanner must retain coordinator commit ordering.
- A lossy mode would require a later explicit policy and permanent metadata.
- An async runtime is unnecessary until measured concurrency demonstrates a
  concrete benefit.

## Rejected alternatives

- **Independent producer writes:** creates ambiguous order and transaction races.
- **Unbounded channels:** allows memory exhaustion.
- **Drop terminal frames under pressure:** silently corrupts replay.
- **Timestamp ordering:** clocks can repeat, jump, or lack sufficient precision.
- **Global `Arc<Mutex<Everything>>`:** hides ownership and lets unrelated work
  block under one product-wide critical section.
