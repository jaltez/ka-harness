#![no_main]

use libfuzzer_sys::fuzz_target;

const GUNZIP_CAP: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if let Ok(entries) = ka_engine::readers::tar_entries(data) {
        let _ = ka_engine::readers::tar_listing(&entries);
    }
    if let Ok(decompressed) = ka_engine::readers::gunzip(data, GUNZIP_CAP) {
        let _ = ka_engine::readers::tar_entries(&decompressed);
    }
});
