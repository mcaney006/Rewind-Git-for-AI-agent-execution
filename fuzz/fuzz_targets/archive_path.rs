#![no_main]

use libfuzzer_sys::fuzz_target;
use rewind_store::validate_archive_path;

const MAX_FUZZ_PATH_BYTES: usize = 8192;

fuzz_target!(|bytes: &[u8]| {
    let bounded = &bytes[..bytes.len().min(MAX_FUZZ_PATH_BYTES)];
    if let Ok(path) = std::str::from_utf8(bounded) {
        let _ = validate_archive_path(path);
    }
});
