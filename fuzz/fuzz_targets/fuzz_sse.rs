#![no_main]

use libfuzzer_sys::fuzz_target;

// `ka_dialect::sse` is crate-private; include the live source directly so we
// fuzz the exact parser without altering `crates/ka-dialect/src/lib.rs`.
#[path = "../../crates/ka-dialect/src/sse.rs"]
mod sse;

fuzz_target!(|data: &[u8]| {
    // 1. Feed the whole buffer at once.
    let mut parser = sse::SseParser::new();
    let _ = parser.feed(data);
    let _ = parser.finish();

    // 2. Feed in small chunks to exercise cross-chunk UTF-8 and separator splits.
    if let Some((&first, rest)) = data.split_first() {
        let step = ((first as usize) % 16) + 1;
        let mut chunked = sse::SseParser::new();
        for chunk in rest.chunks(step) {
            let _ = chunked.feed(chunk);
        }
        let _ = chunked.finish();
    }
});
