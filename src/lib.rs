//! # mcp_io
//!
//! A Model Context Protocol server framework, in development. This commit
//! adds its JSON Schema 2020-12 validator ([`schema`]).

#![forbid(unsafe_code)]
#![deny(
    warnings,
    missing_docs,
    unsafe_op_in_unsafe_fn,
    unused_must_use,
    unused_results,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::todo,
    clippy::unimplemented,
    clippy::panic,
    clippy::unreachable,
    clippy::dbg_macro,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc
)]

pub use rpc_io::{self, jsonx};

pub mod schema;

pub use schema::{Schema, SchemaError, SchemaErrorKind, SchemaLimits, ValidationError, Violations};
