#![no_main]

use libfuzzer_sys::fuzz_target;
use rewind_store::{BundleDecodeLimits, decode_bundle};

const MEBIBYTE: u64 = 1024 * 1024;

fuzz_target!(|bytes: &[u8]| {
    let _ = decode_bundle(
        bytes,
        BundleDecodeLimits {
            maximum_entries: 64,
            maximum_entry_bytes: MEBIBYTE,
            maximum_total_bytes: 4 * MEBIBYTE,
        },
    );
});
