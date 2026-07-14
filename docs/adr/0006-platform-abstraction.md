# ADR 0006: Keep operating-system behavior behind a narrow boundary

- **Status:** Accepted
- **Date:** 2026-07-13
- **Implementation:** Implemented for current clone, no-follow read, PTY,
  terminal, process, path, and disk-space callers. Focused wrappers and real
  capture are verified on macOS; Linux runtime verification remains pending.

## Context

macOS and Linux expose different clone, watcher, PTY, and process-inspection
APIs. Scattered `cfg` branches would couple durable policy to host details and
make unsupported targets appear to work. Some native calls require unsafe FFI.

## Decision

`rewind-platform` owns target selection, capability probes, and the smallest
safe wrappers for filesystem cloning, PTYs, process inspection, watchers, and
disk diagnostics. Product layers receive owned values and distinguish
“unsupported capability” from operational failure.

Target-specific `cfg` and necessary unsafe blocks stay in that crate. Narrow
portable `cfg(unix)` metadata guards do not select platform behavior. Unsafe is
denied elsewhere. Every unsafe block documents required invariants with
`SAFETY`, keeps the raw scope minimal, converts immediately to safe owned types,
and has a focused safe-wrapper test.

The first-class targets are Apple-Silicon macOS and modern x86-64/ARM64 Linux.
Unsupported platforms fail explicitly. Portable recursive copy is a deliberate
capability fallback; failures such as permission denial are not hidden as
unsupported.

## Consequences

- Domain and snapshot invariants can be tested independently of OS APIs.
- Capability diagnostics describe the actual filesystem/host, not assumptions
  based only on target triple.
- macOS and Linux integration tests remain necessary; the abstraction cannot
  prove native wrappers work.
- A platform operation is added only when a product caller needs it, avoiding a
  speculative portability framework.

## Rejected alternatives

- **`cfg` throughout product crates:** spreads policy and unsafe review surface.
- **Pretend every Unix is equivalent:** yields silent partial behavior.
- **Shell out to `cp`, `ps`, or platform utilities:** unstable parsing and weaker
  error/capability control.
- **A trait for every native function:** abstraction without multiple product
  implementations; narrow functions/modules suffice.
