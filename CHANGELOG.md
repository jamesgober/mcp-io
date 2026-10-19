<h1 align="center">
    <img width="90px" height="auto" src="https://raw.githubusercontent.com/jamesgober/jamesgober/main/media/icons/hexagon-3.svg" alt="Triple Hexagon">
    <br><b>CHANGELOG</b>
</h1>
<p>
  All notable changes to <code>mcp-io</code> will be documented in this file. The format is based on <a href="https://keepachangelog.com/en/1.1.0/">Keep a Changelog</a>,
  and this project adheres to <a href="https://semver.org/spec/v2.0.0.html">Semantic Versioning</a>.
</p>

---

## [Unreleased]

---

## [0.2.0] — 2026-10-19

The foundation: an MCP server framework for tools, on MCP 2026-07-28 with the 2025-11-25 and 2025-06-18 handshake revisions on the same connection, a first-party JSON Schema validator, permission tiers enforced through the protocol's own confirmation mechanism, and the stdio transport over rpc-io 0.4. The design follows the 2026-10-14 baseline audit; its decisions and waivers are recorded in `dev/DIRECTIVES.md` §5.

### Added

- **Versioning, both eras.** A request carrying `_meta["io.modelcontextprotocol/protocolVersion"]` is served statelessly (MCP 2026-07-28): `clientCapabilities` required (`-32602` without it), unknown versions refused with `-32022` and the supported list, `clientInfo` and `logLevel` checked, `resultType` and `_meta["io.modelcontextprotocol/serverInfo"]` on every result (`Server::with_server_info_meta`). A connection that opens with `initialize` gets the legacy lifecycle (`Lifecycle`: `Stateless`, `Initializing`, `Operating`) with version negotiation (2025-11-25, 2025-06-18), `notifications/initialized`, and `ping`. `Server::with_legacy(false)` makes a modern-only server, which answers `initialize` with `-32022`.
- `server/discover`: supported versions, capabilities, instructions, `ttlMs`, `cacheScope`.
- **Tools.** `Tool` (name checked against MCP's rule; title, description, input and output schemas, `idempotent`, `open_world`, a confirmation question), `Tier` (`ReadOnly`, `Mutating`, `Destructive`, `Publish`, mapped to tool annotations), and `Tools`, the server's tool set, shared by every connection and changeable while serving.
- `tools/list`: sorted by name, paginated with opaque cursors that survive changes between pages (`Server::with_page_size`; a cursor this server did not issue, malformed or forged, is `-32602`), with caching hints (`Server::with_cache`, `CacheScope`).
- `tools/call`: arguments validated before the handler, tool execution errors (`isError: true`) for invalid arguments, denials, and handler errors, and protocol errors for unknown tools and malformed requests (`-32602`, a repeated key in the params included); `ToolResult` with `Content` (text, image, audio, resource links, embedded resources, `ResourceContents`), structured content checked against the output schema with a text copy (a legacy session is not shown non-object output schemas or structured content, which its revisions forbid, and gets the text copy alone; boolean property schemas are shown to it as `{}` and `{"not": {}}`), `ToolResult::encode` for any jsonx-lib `ToJson` value; `ToolError` (`new`, `protocol`, `missing_capability`, and `From` any `std::error::Error`); handler panics answered with `-32603`.
- **`schema`**: a first-party JSON Schema 2020-12 validator for a documented subset (`Schema`, `SchemaLimits`, `SchemaError`, `SchemaErrorKind`, `ValidationError`, `Violations`), with exact decimal arithmetic for numbers, `O(n log n)` `uniqueItems`, local `$ref`/`$anchor`/`$defs`, and budgets on compilation and validation. Keywords, dialects, and references outside the subset are refused at registration, and so are `$ref` cycles that never descend into the instance (`SchemaErrorKind::ReferenceCycle`). Error paths and messages clip client property names to 64 bytes and paths to 512 bytes.
- **Permissions**: `Policy` (closures included, consulted once per call), `Decision`, `PermissionRequest`, `Ceiling` (the default, `Ceiling(Tier::Mutating)`). Confirm-tier calls ask the user through the client, showing the arguments in full as the tool receives them (members in the order sent, numbers as spelled, non-ASCII and control characters escaped; a call too large to show, `Server::with_max_confirmed_arguments`, 4 KiB by default, is refused rather than shown in part): on 2026-07-28 an `InputRequiredResult` with a form `elicitation/create` and a single-use, server-side `requestState` bound to the tool and its exact arguments; on a legacy session a server-sent `elicitation/create`, abandoned (and withdrawn) if the client cancels the call. Confirmations are bound to the tool's registration and the exact text shown (a retry spelling the arguments otherwise is refused), keep only a keyed digest of it, are compared in constant time, and are capped per connection. `-32021` when the client cannot elicit. `Publish` tools always ask. `Server::with_confirmation_timeout`, `with_max_pending_confirmations`.
- **Audit trail**: `Server::on_audit` with an `AuditRecord` (tool, tier, id, era, client name, `Outcome`, elapsed time; never arguments or results) for every `tools/call`, refused requests included (`Outcome::Refused`, with no tool, tier, or era when the request did not name them).
- **Typed tools**: `ToolCall::decode::<T>()` reads the arguments as any jsonx-lib `FromJson` type from the request's own text, borrowing strings with no copy where the type borrows (`&str`, `Cow<str>`); absent or `null` arguments read as `{}`; arguments that pass the schema but not the type are a tool execution error naming where. `Server::tool_json` and `Tools::add_json` register a tool whose arguments are decoded into an owned type and whose `ToJson` result is sent as structured content. `JsonSchema`, with `Tool::input_schema_for` and `output_schema_for`, keeps a type's schema next to its decoder. The `Value` path (`ToolCall::arguments`) is unchanged.
- **Cancellation and progress**: tool calls run on a bounded worker pool per connection (`Server::with_workers`, `with_max_queued_calls`, `with_max_queued_bytes`), so `notifications/cancelled` reaches a running call (`ToolCall::is_cancelled`, `cancel_token`) and it is not answered. `ToolCall::progress` sends progress only when asked, refuses values that do not increase and reports after cancellation.
- **`subscriptions/listen`**: acknowledgment first, notifications tagged with `io.modelcontextprotocol/subscriptionId` and sent through rpc-io's request-bound `Notifier` (refused once the stream is answered or cancelled, under the lock its response is written under, so no list change follows a stream's end), `toolsListChanged` honoured, cancellation by `notifications/cancelled`, graceful closure on a server-side `Connection::close` (the listen response, then `notifications/cancelled`), a per-connection limit (`Server::with_max_subscriptions`). Legacy sessions get plain `notifications/tools/list_changed`.
- **Transport**: `Server::serve` and `serve_stdio` (newline-delimited JSON-RPC through rpc-io's bounded output queue), and `Connection` over any rpc-io `Sink` (`handle`, `run`, `wait_idle`, `close`, `lifecycle`, `peer`, `stats`). The end of input handles a last unterminated line and waits for running calls (`Server::with_drain_timeout`). `Server::with_limits` sets message budgets; batches stay refused.
- `protocol`: revision constants, the reserved `_meta` keys, `UNSUPPORTED_PROTOCOL_VERSION`, `MISSING_REQUIRED_CLIENT_CAPABILITY`, `HEADER_MISMATCH`, `Era`, `unsupported_version`, `missing_capability`.
- `Implementation` (`serverInfo`), `Error`, `RegisterError`; re-exports `mcp_io::jsonx` (jsonx-lib) and `mcp_io::rpc_io`.
- `examples/stdio_server.rs`: a server with a tool per tier.
- Tests: lifecycle, tools, permission, surface, and stdio suites (the stdio suite runs the example as a child process); wire conformance against the official MCP JSON Schemas (2026-07-28, 2025-11-25, and 2025-06-18, vendored under `tests/data/mcp-schema/`) from the specification's examples, with object and non-object tool outputs, validated by the crate's own validator; the JSON Schema Test Suite (draft 2020-12, vendored under `tests/data/json-schema-test-suite/`); property tests against reference implementations; fuzz targets `server` and `schema` (`fuzz/`), with their checks in `cargo test`; criterion 0.7 benchmarks.

### Changed

- Dependencies: `rpc-io` 0.4, first-party, the only one; JSON is jsonx-lib 0.5 through rpc-io's re-export, so the graph holds one jsonx-lib.
- The crate description, README, and `docs/API.md` describe what exists, and say what the permission gate cannot guarantee (audit MCP-DOC-01, MCP-ARC-03).
- Crate-root lints follow REPS: `warnings`, `unsafe_op_in_unsafe_fn`, `clippy::unreachable`, `clippy::panic`, `clippy::undocumented_unsafe_blocks`, `clippy::missing_safety_doc` added (MCP-STD-01).
- `Cargo.toml` `include` keeps the engineering notes, tests, and CI out of the package (MCP-DOC-05). `rustfmt.toml` drops its nightly-only options (MCP-HLT-01). `.gitattributes` keeps `docs/` and `tests/` in source archives (MCP-HLT-02).
- CI: `permissions: contents: read`, a weekly security run, the MSRV and 1.99 toolchains, the fuzz smoke run on the `x86_64-unknown-linux-gnu` target, coverage, and examples (MCP-HLT-03).

### Removed

- The empty `std` feature and the `no_std` attribute: a server needs threads and stdio (MCP-ARC-10).
- `panic = "abort"` from the release profile, which would have killed an example server on a handler panic instead of containing it (MCP-STD-07).

### Security

- Nothing outside a tool's schema reaches its handler; budgets bound schema compilation (depth, subschemas) and validation (steps, including the bytes `enum`, `const`, and `uniqueItems` compare, and nesting), so a hostile schema or input costs bounded work. Numbers beyond the validator's exact range are refused, not compared approximately.
- Memory a client can pin is bounded: queued calls by count and bytes, pending confirmations per connection (a digest each), list-change watchers by live streams.
- Network `$ref`s are never fetched; schemas with one are refused.
- Validation errors name locations, never argument values; the audit trail never records arguments; client text quoted in errors is clipped to 128 bytes, and client property names in validation errors to 64.
- A confirmation never shows less than what runs: the arguments in full or a refusal, bound to the text shown.

---

## [0.1.0] - 2026-10-09

Initial scaffold and repository bootstrap. No domain logic yet &mdash; this release establishes the structure, tooling, and quality gates the implementation will be built on. It was never tagged or published.

### Added

- `Cargo.toml` with crate metadata, Rust 2024 edition, MSRV 1.85.
- Dual `Apache-2.0 OR MIT` license files.
- `README.md`, `CHANGELOG.md`, and a documentation skeleton.
- `REPS.md` compliance baseline.
- `.github/workflows/ci.yml` CI matrix; `deny.toml`, `clippy.toml`, `rustfmt.toml`.
- `dev/DIRECTIVES.md` and `dev/ROADMAP.md` (committed engineering standards + plan).

[Unreleased]: https://github.com/jamesgober/mcp-io/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jamesgober/mcp-io/releases/tag/v0.2.0
[0.1.0]: https://github.com/jamesgober/mcp-io/commit/a85830e0b044a1b14c52efbcfc57e496c5f4d3ae
