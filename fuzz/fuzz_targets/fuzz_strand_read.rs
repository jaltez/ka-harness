#![no_main]

use std::fs;
use std::sync::OnceLock;
use libfuzzer_sys::fuzz_target;

static TEMP_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let path = TEMP_PATH.get_or_init(|| {
        std::env::temp_dir().join(format!("ka-fuzz-strand-{}.jsonl", std::process::id()))
    });
    if fs::write(path, data).is_ok() {
        if let Ok(records) = ka_strand::read(path) {
            let _ = ka_strand::context_window(&records);
        }
    }
});
