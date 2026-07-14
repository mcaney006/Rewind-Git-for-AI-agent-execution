#![no_main]

use libfuzzer_sys::fuzz_target;
use rewind_domain::Event;

const MAX_FUZZ_EVENT_BYTES: usize = 64 * 1024;

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() <= MAX_FUZZ_EVENT_BYTES {
        let _ = serde_json::from_slice::<Event>(bytes);
    }
});
