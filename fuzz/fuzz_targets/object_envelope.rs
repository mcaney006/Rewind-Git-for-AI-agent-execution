#![no_main]

use libfuzzer_sys::fuzz_target;
use rewind_store::decode_object_envelope;

const MAX_FUZZ_ENVELOPE_BYTES: usize = 1024 * 1024 + 16;

fuzz_target!(|bytes: &[u8]| {
    let bounded = &bytes[..bytes.len().min(MAX_FUZZ_ENVELOPE_BYTES)];
    let _ = decode_object_envelope(bounded);
});
