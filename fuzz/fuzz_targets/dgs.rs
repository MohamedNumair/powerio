//! Malformed-input fuzzing of the DIgSILENT DGS ASCII reader (a hand-written
//! table tokenizer).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = powerio::parse_str(text, "dgs");
    }
});
