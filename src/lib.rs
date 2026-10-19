//! # mcp_io
//!
//! A Model Context Protocol server framework: register tools with JSON
//! Schema input, gate them by permission tier, and serve them over stdio, on
//! top of the owner's JSON-RPC engine, [`rpc_io`], whose JSON library is
//! re-exported as [`jsonx`].
//!
//! It implements MCP **2026-07-28** (stateless: every request carries its
//! protocol version and the client's capabilities in `_meta`) and, on the
//! same connection, the **2025-11-25** and **2025-06-18** revisions for
//! clients that open with the `initialize` handshake.
//!
//! ## Quick start
//!
//! ```no_run
//! use mcp_io::{Implementation, Server, Tier, Tool, ToolResult, jsonx::json};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     Server::new(Implementation::new("greeter", "1.0.0"))
//!         .tool(
//!             Tool::new("greet", Tier::ReadOnly)
//!                 .description("Greets someone by name")
//!                 .input_schema(json!({
//!                     "type": "object",
//!                     "properties": { "name": { "type": "string", "minLength": 1 } },
//!                     "required": ["name"],
//!                     "additionalProperties": false
//!                 })),
//!             |call| Ok(ToolResult::text(format!("Hello, {}!", call.arguments()["name"].as_str().unwrap_or("you")))),
//!         )?
//!         .serve_stdio()?; // until the client closes standard input
//!     Ok(())
//! }
//! ```
//!
//! ## What a server does for you
//!
//! - **Versioning, both eras.** A request with
//!   `_meta["io.modelcontextprotocol/protocolVersion"]` is served on its own:
//!   `server/discover`, `tools/list`, `tools/call`, `subscriptions/listen`,
//!   with `resultType` and `serverInfo` on every result, `-32602` for missing
//!   per-request fields and `-32022` (with the supported versions) for an
//!   unknown version. A connection that opens with `initialize` gets the
//!   legacy lifecycle ([`Lifecycle`]) and `ping`.
//! - **Tools.** A registry shared by every connection ([`Tools`]), sorted
//!   for deterministic, cursor-paginated `tools/list` with caching hints, and
//!   changeable while serving (`notifications/tools/list_changed` to clients
//!   that asked).
//! - **Validation.** Arguments are checked against the tool's input schema
//!   before the handler runs, structured results against its output schema
//!   after. The validator is this crate's own, for a documented subset of
//!   JSON Schema 2020-12 ([`schema`]); a schema outside the subset is refused
//!   at registration, never half-applied. Arguments can also be read as
//!   typed values ([`ToolCall::decode`], [`Server::tool_json`]), borrowing
//!   from the request where the type does.
//! - **Errors as MCP wants them.** Invalid arguments, denials, and handler
//!   failures are tool results with `isError: true` that the model can read;
//!   unknown tools and malformed requests are JSON-RPC errors. A panicking
//!   handler is answered with `-32603`; the server goes on.
//! - **Permission tiers.** Every call passes the [`permission`] gate: the
//!   host's [`Policy`], then, for confirm-tier calls, the user's answer to an
//!   elicitation sent through the client. `Publish` tools always ask.
//! - **Cancellation and progress.** Tool calls run on worker threads, so a
//!   `notifications/cancelled` stops one (the handler sees
//!   [`ToolCall::is_cancelled`]; no answer is sent). Progress reports must
//!   increase and cannot outlive the call.
//! - **Hostile input.** rpc-io's budgets on message size and JSON depth,
//!   bounded schema compilation and validation, bounded queues, no batches,
//!   and no panics on any input (fuzzed).
//!
//! ## What this crate does not claim
//!
//! - The gate cannot prove a human answered a confirmation: the client
//!   relays it. See [`permission`].
//! - Resources, prompts, completion, the Tasks extension, the Streamable
//!   HTTP transport, and the deprecated Logging, Roots, and Sampling
//!   features are not implemented yet (see the roadmap).
//! - JSON Schema keywords outside the subset (`pattern`, `unevaluated*`,
//!   dynamic references) are refused, not validated.
//!
//! Standard output belongs to the protocol: this crate writes nothing but
//! MCP messages to it, and does not print to standard error either (a
//! panicking tool handler is still reported there by Rust's panic hook,
//! unless the host replaces the hook). Log to standard error yourself.

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

mod audit;
mod call;
mod content;
mod error;
mod json;
pub mod permission;
mod pool;
pub mod protocol;
mod registry;
pub mod schema;
mod server;
mod session;
mod tool;

pub use audit::{AuditRecord, Outcome};
pub use call::ToolCall;
pub use content::{Content, ResourceContents, ToolError, ToolResult};
pub use error::{Error, RegisterError};
pub use permission::{Ceiling, Decision, PermissionRequest, Policy};
pub use registry::Tools;
pub use schema::{Schema, SchemaError, SchemaErrorKind, SchemaLimits, ValidationError, Violations};
pub use server::{CacheScope, Connection, Implementation, Server};
pub use session::Lifecycle;
pub use tool::{JsonSchema, Tier, Tool};

/// Compiles and runs the `rust` code blocks in `README.md` and `docs/API.md`
/// as part of `cargo test`, so the published examples cannot drift from the
/// API. Present only while collecting doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
#[doc = include_str!("../docs/API.md")]
pub struct MarkdownDocTests;
