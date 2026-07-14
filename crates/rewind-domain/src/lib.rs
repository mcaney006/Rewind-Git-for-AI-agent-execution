//! Durable, platform-neutral types and invariants for Rewind.
//!
//! This crate deliberately contains no persistence, terminal, CLI, or
//! operating-system integration. Sequence numbers, rather than timestamps,
//! define event order.

#![deny(missing_docs)]

mod comparison;
mod event;
mod id;
mod run;
mod snapshot;
mod time;

pub use comparison::*;
pub use event::*;
pub use id::*;
pub use run::*;
pub use snapshot::*;
pub use time::*;
