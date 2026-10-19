//! Arbitrary bytes as a JSON Schema and an instance (split at the first
//! `0x00`): compiling and validating never panic and stay within budget.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../tests/harness/mod.rs"]
mod harness;

fuzz_target!(|data: &[u8]| harness::check_schema(data));
