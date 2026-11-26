#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(line) = std::str::from_utf8(data) {
        let analysis = ka_engine::hands::bashp::analyze(line);
        let _ = ka_engine::hands::bashp::all_readonly(&analysis);
        let _ = ka_engine::hands::bashp::hardstop(line, &analysis);
    }
});
