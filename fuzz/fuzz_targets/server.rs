//! Arbitrary bytes, split into lines, through a live connection: every
//! method, both eras, the permission gate, and malformed input.

#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

#[path = "../../tests/harness/mod.rs"]
mod harness;

fuzz_target!(|data: &[u8]| {
    static SERVER: OnceLock<mcp_io::Server> = OnceLock::new();
    harness::check_server(SERVER.get_or_init(harness::server), data);
});
