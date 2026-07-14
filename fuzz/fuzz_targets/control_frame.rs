#![no_main]

use libfuzzer_sys::fuzz_target;
use rewind_capture::decode_control_frame;

fuzz_target!(|bytes: &[u8]| {
    let _ = decode_control_frame(bytes);
});
