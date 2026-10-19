//! # mcp_io
//!
//! Model Context Protocol server framework: tools, resources, prompts, permission tiers, and stdio/HTTP transports.
//!
//! Scaffold release (v0.1.0). The public surface is designed across the 0.x
//! series and frozen at v1.0. See `docs/API.md` and `dev/ROADMAP.md` for the
//! current phase scope.

#![cfg_attr(docsrs, feature(doc_cfg))]
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

#[cfg(test)]
mod tests {
    #[test]
    fn test_scaffold_compiles_as_mcp_io() {
        assert_eq!(env!("CARGO_PKG_NAME"), "mcp-io");
    }
}
