# ADR 0007: Redact input when terminal echo is disabled

- **Status:** Accepted
- **Date:** 2026-07-13
- **Implementation:** Implemented beside the stdin read using an independent PTY
  echo probe; the sampled decision travels with the input observation and fails
  closed when unavailable. Terminal restoration and the retention decision have
  focused tests; full interactive secret-prompt automation remains pending.

## Context

Interactive commands request passwords and tokens through the PTY. Recording
all input by default would make a local execution trace a credential archive.
Recording no input harms ordinary replay. Terminal echo state is useful but can
change concurrently and is not a reliable secret classifier.

## Decision

Expose `--record-input=auto|always|never` and default to `auto`.

- `auto` stores forwarded bytes only while the child terminal reports echo
  enabled. With echo disabled it stores `TerminalInputRedacted` containing
  timing and byte length, not bytes.
- `always` stores all input after explicit user selection.
- `never` stores no input bytes and is the recommended policy for sensitive
  interactive sessions.

Input bytes and file contents never enter tracing logs. The resolved policy is
durable run metadata. Documentation and CLI warnings state that echo detection
is best effort and that output can still reveal typed secrets.

## Consequences

- Common no-echo password prompts avoid raw input capture by default.
- Ordinary echoed interaction remains replayable.
- Timing and byte length leak limited metadata even for redacted events.
- The input producer must sample echo immediately beside the read, before
  forwarding, and carry that decision through bounded backpressure. Terminal
  restoration failures must remain visible.
- Export preserves redaction and cannot reconstruct the missing bytes.

## Rejected alternatives

- **Always record input:** unsafe default for developer credentials.
- **Never record input by default:** unnecessarily degrades ordinary replay.
- **Parse prompts for words such as “password”:** language- and program-specific
  heuristic with dangerous false negatives.
- **Claim echo detection is infallible:** races and custom input handling make
  that guarantee impossible.
