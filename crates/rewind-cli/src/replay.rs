use std::io::{self, IsTerminal, Write};
use std::path::Path;

use rewind_capture::MAX_TERMINAL_CHUNK_BYTES;
use rewind_domain::{EventPayload, EventSequence, RunId};
use rewind_store::{Store, StoreError};
use thiserror::Error;

const REPLAY_PAGE_SIZE: u32 = 512;

#[derive(Debug, Error)]
pub(crate) enum ReplayError {
    #[error("cannot write recorded terminal output: {0}")]
    Output(#[from] io::Error),
    #[error("native replay failed: {0}")]
    Tui(String),
    #[error(
        "refusing to write raw recorded escape sequences to a terminal without interactive input; run from a terminal or redirect stdout"
    )]
    UnsafeRawTerminal,
    #[error("terminal object {object_id} is {actual} bytes; event declares {expected}")]
    TerminalLength {
        object_id: rewind_domain::ObjectId,
        expected: u64,
        actual: u64,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub(crate) fn replay(
    store_root: &Path,
    store: &Store,
    run_id: RunId,
    terminal_cache_bytes: u64,
) -> Result<(), ReplayError> {
    if io::stdout().is_terminal() {
        if io::stdin().is_terminal() {
            let cache = usize::try_from(terminal_cache_bytes).unwrap_or(usize::MAX);
            return rewind_tui::replay(store_root, run_id, cache)
                .map_err(|error| ReplayError::Tui(error.to_string()));
        }
        return Err(ReplayError::UnsafeRawTerminal);
    }
    stream_terminal(store, run_id)
}

fn stream_terminal(store: &Store, run_id: RunId) -> Result<(), ReplayError> {
    let mut cursor: Option<EventSequence> = None;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    loop {
        let page = store.load_timeline(run_id, cursor, REPLAY_PAGE_SIZE)?;
        for event in &page.events {
            if let EventPayload::TerminalOutput {
                object_id,
                byte_len,
                ..
            } = &event.payload
            {
                let bytes = store.load_object(*object_id, MAX_TERMINAL_CHUNK_BYTES as u64)?;
                let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if actual != *byte_len {
                    return Err(ReplayError::TerminalLength {
                        object_id: *object_id,
                        expected: *byte_len,
                        actual,
                    });
                }
                stdout.write_all(&bytes)?;
            }
            cursor = Some(event.sequence);
        }
        if !page.has_more {
            break;
        }
    }
    stdout.flush()?;
    Ok(())
}
