<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>mcp-io</b>
    <br>
    <sub><sup>MCP SERVER FRAMEWORK</sup></sub>
</h1>

<div align="center">
    <a href="https://crates.io/crates/mcp-io"><img alt="Crates.io" src="https://img.shields.io/crates/v/mcp-io"></a>
    <a href="https://crates.io/crates/mcp-io"><img alt="Downloads" src="https://img.shields.io/crates/d/mcp-io?color=%230099ff"></a>
    <a href="https://docs.rs/mcp-io"><img alt="docs.rs" src="https://img.shields.io/docsrs/mcp-io"></a>
    <a href="https://github.com/jamesgober/mcp-io/actions"><img alt="CI" src="https://github.com/jamesgober/mcp-io/actions/workflows/ci.yml/badge.svg"></a>
    <a href="https://github.com/rust-lang/rfcs/blob/master/text/2495-min-rust-version.md"><img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85%2B-blue"></a>
</div>

<br>

<div align="left">
    <p>
        <strong>mcp-io</strong> builds Model Context Protocol servers. Declare tools with a JSON Schema for their input and a permission tier, and serve them over stdio: the crate handles MCP's per-request versioning, discovery, paginated tool lists, argument validation, tool errors versus protocol errors, cancellation, progress, list-change subscriptions, and asking the user before a dangerous tool runs. It speaks MCP <strong>2026-07-28</strong> (stateless) and, on the same connection, <strong>2025-11-25</strong> and <strong>2025-06-18</strong> for clients that still open with <code>initialize</code>.
    </p>
    <p>
        It runs on <a href="https://crates.io/crates/rpc-io"><code>rpc-io</code></a>, the owner's JSON-RPC engine, which brings hostile-input budgets, bounded output queues, and panic containment. Tool calls run on a small worker pool, so a slow tool never stops the server from reading the cancellation for it. There is no async runtime, no <code>serde</code>, and no proc-macro.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition). <code>#![forbid(unsafe_code)]</code>. One dependency, from the family: <code>rpc-io</code>, which brings <code>jsonx-lib</code> (re-exported here as <code>mcp_io::jsonx</code>).
    </p>
    <blockquote>
        <strong>Status: pre-1.0.</strong> 0.2.0 is the foundation: tools, the two protocol eras, the permission gate, and the stdio transport. Resources, prompts, and the HTTP transport come later in the 0.x series; the API is frozen at <code>1.0.0</code>. See <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a> and <a href="./dev/ROADMAP.md"><code>dev/ROADMAP.md</code></a>.
    </blockquote>
</div>

<hr>
<br>

## Installation

```toml
[dependencies]
mcp-io = "0.2"
```

`mcp_io::jsonx` re-exports jsonx-lib (for `json!` and `Value`), and `mcp_io::rpc_io` the engine.

<br>

## Quick start

```rust,no_run
use mcp_io::{Implementation, Server, Tier, Tool, ToolError, ToolResult, jsonx::json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    Server::new(Implementation::new("adder", "1.0.0"))
        .instructions("Use `add` for integer sums.")
        .tool(
            Tool::new("add", Tier::ReadOnly)
                .description("Adds two integers")
                .input_schema(json!({
                    "type": "object",
                    "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                    "required": ["a", "b"],
                    "additionalProperties": false
                })),
            |call| {
                // The arguments have passed the schema before this runs.
                let a = call.arguments()["a"].as_i64().ok_or_else(|| ToolError::new("`a` must fit in 64 bits"))?;
                let b = call.arguments()["b"].as_i64().ok_or_else(|| ToolError::new("`b` must fit in 64 bits"))?;
                let sum = a.checked_add(b).ok_or_else(|| ToolError::new("the sum overflows 64 bits"))?;
                Ok(ToolResult::text(sum.to_string()))
            },
        )?
        .serve_stdio()?; // until the client closes standard input
    Ok(())
}
```

A runnable server with a tool in each tier is [`examples/stdio_server.rs`](./examples/stdio_server.rs) (`cargo run --example stdio_server`).

<br>

## Two protocol eras on one connection

| The client sends | The server |
|---|---|
| A request with `_meta["io.modelcontextprotocol/protocolVersion"]: "2026-07-28"` and `clientCapabilities` | Serves it on its own: no state from earlier requests. Results carry `resultType` and `_meta["io.modelcontextprotocol/serverInfo"]`. |
| `server/discover` | `supportedVersions`, `capabilities`, `instructions`, `ttlMs`, `cacheScope`. |
| A request with an unknown version | `-32022` with `{"supported": ["2026-07-28"], "requested": ...}`. |
| A request missing `protocolVersion` or `clientCapabilities` (and no legacy session) | `-32602`. |
| `initialize` (2025-11-25 or 2025-06-18; anything else is offered 2025-11-25) | A legacy session for the connection: `ping`, `notifications/initialized`, list changes as plain notifications. `Server::with_legacy(false)` answers it with `-32022` instead. |
| A JSON-RPC batch | `-32600`: MCP has had no batches since 2025-06-18. |

<br>

## Tools, validation, and errors

- **Listing** is sorted by name, paginated with opaque cursors (`Server::with_page_size`, a cursor this server did not issue is `-32602`), and carries `ttlMs`/`cacheScope` (`Server::with_cache`).
- **Arguments** are validated against the tool's `inputSchema` before the handler runs. A failure is a tool result with `isError: true` that names each problem by JSON Pointer, so the model can fix its call. It never quotes argument values; the paths hold property names, each clipped to 64 bytes (paths to 512, the whole report to 8 KiB).
- **Results**: text, image, audio, resource links, embedded resources, and `structuredContent`. With an `outputSchema`, structured content must match it, and a text copy is added for clients that read only `content`.
- **Errors**: a handler's `ToolError` is a tool execution error (`isError`); an unknown tool or malformed request is `-32602`; a panic is `-32603` and the server goes on. `ToolError::missing_capability` answers MCP's `-32021`.
- **Typed tools**: `ToolCall::decode::<T>()` reads the arguments as any jsonx-lib `FromJson` type, borrowing strings from the request with no copy where the type borrows; `Server::tool_json` registers a tool with an owned argument type and a `ToJson` result (structured content); `JsonSchema` keeps a type's schema next to its decoder (`Tool::input_schema_for`).
- **Changes while serving**: `Server::tools()` adds and removes tools; clients with a `subscriptions/listen` stream (`toolsListChanged`) or an initialized legacy session get `notifications/tools/list_changed`.

The JSON Schema validator is the crate's own and covers a documented subset of 2020-12: types, `enum`/`const`, numeric bounds and `multipleOf` (exact decimal arithmetic), string and array lengths, `prefixItems`/`items`/`contains`, `properties`/`required`/`additionalProperties`/`propertyNames`/`dependentRequired`/`dependentSchemas`, `allOf`/`anyOf`/`oneOf`/`not`/`if`, and local `$ref`/`$anchor`/`$defs`. Schemas that use `pattern`, `patternProperties`, `unevaluated*`, dynamic references, another dialect, a reference outside the document, or a `$ref` cycle that never descends into the instance (such as `{"$ref": "#"}`) are refused at registration. On the official JSON Schema Test Suite (draft 2020-12), it gets every case of every schema it accepts right; the [`schema` module docs](./docs/API.md#schema) list exactly what is refused.

<br>

## Permission tiers

Every tool declares a tier, and every call passes a gate after validation and before the handler:

| Tier | Annotations | Default policy |
|---|---|---|
| `ReadOnly` | `readOnlyHint: true` | runs |
| `Mutating` | `readOnlyHint: false`, `destructiveHint: false` | runs |
| `Destructive` | `destructiveHint: true` | asks the user first |
| `Publish` | `destructiveHint: true`, `openWorldHint: true` | always asks the user first; no policy can waive it |

The host's `Policy` decides each call once (`Allow`, `Deny` with a reason, or `Confirm`; the retry that carries a confirmation is not decided again); `Ceiling(Tier::Mutating)` is the default. The question shows the call's arguments in full (canonical JSON, non-ASCII escaped); a call too large to show in full (`Server::with_max_confirmed_arguments`, 4 KiB) is refused, never shown in part. A confirmation uses MCP's own mechanism: on 2026-07-28 the call is answered with an `InputRequiredResult` holding a form `elicitation/create` and a single-use `requestState`, and the tool runs when the client retries with the user's acceptance; on a legacy session the server sends `elicitation/create` and waits. A client that cannot elicit gets `-32021` (or a tool error on a legacy session), and the tool does not run. Every call is reported to `Server::on_audit` (tool, tier, id, client name, outcome; never the arguments).

What the gate guarantees, and what it does not: a model cannot run a tool the policy denies, cannot run a confirm-tier tool without a confirmation the server issued for that exact tool and those exact arguments, and cannot use one confirmation twice. The answer itself comes through the client, and the server cannot verify that a person, rather than an automated client, gave it. Annotations are hints a client must not trust. A handler that runs has the server process's privileges: the gate is not a sandbox.

<br>

## Performance

`cargo bench --bench bench`, measured on a Windows development machine under background load (criterion's 95% intervals; expect other machines to differ):

| Benchmark | Time |
|---|---|
| Validate typical tool arguments (a 5-property schema with an enum, bounds, and a unique-items array) | 0.53 to 0.59 µs |
| `uniqueItems` over 1,000 / 100,000 strings | 77 to 82 µs / 13 to 15 ms |
| One `tools/list` page of 100 tools, from a registry of 1,000 or of 100,000 | 133 to 149 µs (the same for both sizes) |
| A whole `tools/call` through a connection (parse, validate, gate, handler, result), inline, with 1 or 100,000 tools registered | 4.4 to 5.7 µs |

<br>

## How it is tested

| Property | How it is checked |
|---|---|
| Messages match the specification. | Every message in the conformance scenarios is validated against the official MCP JSON Schema of its revision (2026-07-28, 2025-11-25, or 2025-06-18), as the official conformance suite's wire check does, including tools whose output is not an object; the requests come from the specification's examples. The validator in `cargo test` is the crate's own; the release verification repeated the check on the stdio server with python-jsonschema (2026-07-28 and 2025-11-25). The official suite itself is not run: it drives servers over HTTP only. |
| The validator answers correctly. | The JSON Schema Test Suite (draft 2020-12): no wrong answer for any schema it accepts; property tests against reference implementations for value equality, `uniqueItems`, numeric bounds, and `multipleOf`. |
| Every request gets one answer, or none when cancelled. | Property tests over random request scripts, and lifecycle, tool, and permission suites covering every refusal. |
| No input makes it panic. | Fuzz targets for the message surface and the schema compiler (`fuzz/`), whose checks also run in `cargo test` on random and mutated input. |
| The stdio transport works end to end. | A test runs the example as a child process, and in-memory pipes check split reads, an unterminated last line, and the drain on end of input. |

<br>

## Contributing

See <a href="./dev/DIRECTIVES.md"><code>dev/DIRECTIVES.md</code></a> for engineering standards and the definition of done. Before a PR: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` must be clean.

<br>

<div id="license">
    <h2>License</h2>
    <p>Licensed under either of</p>
    <ul>
        <li><b>Apache License, Version 2.0</b> &mdash; <a href="./LICENSE-APACHE">LICENSE-APACHE</a></li>
        <li><b>MIT License</b> &mdash; <a href="./LICENSE-MIT">LICENSE-MIT</a></li>
    </ul>
    <p>at your option.</p>
</div>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>James Gober.</strong></sup>
</div>
