# mcp-io - Roadmap

> Path from scaffold to a stable 1.0. Hard parts are front-loaded; each phase has hard exit criteria.
> Master plan: C:/Dev/libraries/Rust/lang-collection/_lexersketch/ROADMAP.md and NEW-LIBS.md
>
> **Anti-deferral rule:** no listed hard task moves to a later phase unless this file records the move and the reason.

## v0.1.0 - Scaffold (DONE)
Compiles, CI green, structure correct, no domain logic.
- [x] Manifest, README, CHANGELOG, REPS, dual license, CI, deny, clippy, rustfmt, DIRECTIVES, ROADMAP.

## v0.2.0 - Foundation (DONE, 2026-10-19)
- [x] ~~Lifecycle and capabilities~~ restated for MCP 2026-07-28, which removed the handshake: per-request `_meta` versioning and capabilities, `server/discover`, `resultType` and `serverInfo` on results, the `-32020..-32099` error codes; plus the legacy `initialize` lifecycle (2025-11-25, 2025-06-18) for clients that open with it (dual-era, `dev/DIRECTIVES.md` §5).
- [x] Tool registry with schemas: validated JSON Schema input and output.
- [x] Permission tiers.
- [x] stdio transport via rpc-io.
- [x] The findings of the 2026-10-14 baseline audit (`_audits/2026-10-14/mcp-io.md`, outside the repo) fixed or recorded in `dev/DIRECTIVES.md` §5.

Delivered:
- Versioning: `protocol` (revisions, reserved `_meta` keys, `-32022`, `-32021`, `-32020`), per-request classification, `server/discover` with caching hints, `Lifecycle` (`Stateless`, `Initializing`, `Operating`), `initialize` negotiation, `notifications/initialized`, legacy `ping`, a modern-only switch (`with_legacy(false)`).
- Tools: `Tool`, `Tier`, `Tools` (shared, changeable while serving), sorted cursor-paginated `tools/list` with `ttlMs`/`cacheScope`, `tools/call` with tool execution errors (`isError`) versus protocol errors, `ToolResult`, `Content` (text, image, audio, resource link, embedded resource), structured content checked against the output schema with a text copy, `ToolError` (including `-32021`), panic containment.
- `schema`: a first-party JSON Schema 2020-12 validator for a documented subset, with exact decimal numbers, `O(n log n)` `uniqueItems`, local references and anchors, compile and validation budgets, and refusal of everything outside the subset at registration.
- Permissions: `Policy`, `Decision`, `Ceiling`, `PermissionRequest`; confirmation through MRTR form elicitation with a single-use, server-side `requestState` (2026-07-28) and through server-sent `elicitation/create` (legacy); `-32021` without elicitation; `Publish` always confirmed; `AuditRecord`/`Outcome` through `on_audit`.
- Execution: a bounded worker pool per connection (cancellation reaches running calls), `ToolCall` with cancellation and request-bound, increasing progress; `subscriptions/listen` with acknowledgment, `subscriptionId` tagging, `toolsListChanged`, and graceful closure on a server-side close; plain list changes for legacy sessions.
- Transport: `Server::serve`/`serve_stdio` on rpc-io's newline framing and output queue, `Connection` over any `Sink`; end of input handles a last unterminated line and drains running calls (bounded by `with_drain_timeout`).
- Tests: lifecycle, tools, permission, and transport suites; wire conformance against the official MCP JSON Schemas (2026-07-28, 2025-11-25) from the spec's examples; the JSON Schema Test Suite; property tests against reference implementations; an end-to-end child-process test; fuzz targets (`server`, `schema`) with their checks in `cargo test`; criterion benchmarks.

Moved, per the anti-deferral rule:
- **Progress and cancellation** (listed under v0.5.0) were delivered here: 2026-07-28 makes them part of serving any tool call (cancellation is how a stdio client abandons a call, and progress is request-scoped), and rpc-io 0.4 provides the request-bound progress handle.
- **`subscriptions/listen` and list-changed notifications** (v0.5.0's "notifications") were delivered here for tools: 2026-07-28 delivers list changes only on a listen stream, and advertising `listChanged` without one would be false.
- **Fuzzing** (listed under v0.9.0) was delivered here: DIRECTIVES §3.6 requires a fuzz target for every parser of untrusted input, and the `_meta` handling and schema compiler are such parsers.

Deferred, with reasons:
- **Typed tool arguments through jsonx-lib's `FromJson`** (`ToolCall::decode::<T>()` and a typed builder deriving schema and decoder from one definition): jsonx-lib 0.5's `FromJson` had not landed when 0.2.0 was finished; `ToolResult::encode` covers `ToJson` output. Moves to v0.3.0.
- **`pattern` and `patternProperties`**: need a regular-expression engine (ECMA-262 dialect); there is no first-party one and DIRECTIVES forbids a rented one. Schemas using them are refused at registration. Moves to v0.5.0, or earlier if a first-party regex crate lands.
- **`unevaluatedProperties`/`unevaluatedItems`, `$dynamicRef`, nested `$id`**: need annotation collection and base-URI tracking across the validator; refused at registration until then. v0.9.0.
- **The official MCP conformance suite in CI** (audit MCP-TST-01): its server runner drives servers over HTTP only (`--url`); it cannot run against stdio. Until the HTTP transport exists, `tests/conformance.rs` performs its wire-schema check against the official schemas. Moves to v0.5.0 with HTTP.
- **Resources, prompts, completion** (audit Should): the registry design takes them, but they are beyond the 0.2.0 milestone; v0.5.0, as planned.
- **A `tracing` feature**: optional instrumentation, as rpc-io's; v0.5.0.
- **URL-mode elicitation and the Tasks extension**: v0.5.0 or later.
- **`icons` on tools and on `Implementation`, and tool `_meta`** (audit Nice-to-have): optional display metadata; v0.5.0, with resources and prompts, which carry the same fields.
- **The `extensions` map in capabilities** (audit MCP-ARC-09): no extension is implemented, so none is advertised; capabilities are rendered internally, so adding the map later is not a breaking change.

## v0.3.0 - Typed tools
- [ ] Typed arguments through jsonx-lib 0.5's `FromJson` (moved from v0.2.0, see above).
- [ ] A typed tool builder: the input schema and the decoder from one definition.

## v0.5.0 - Implementation
- [ ] Resources (`resources/list`, `resources/templates/list`, `resources/read`, `resourceSubscriptions` on `subscriptions/listen`) and prompts (`prompts/list`, `prompts/get`, `promptsListChanged`).
- [ ] Completion (`completion/complete`).
- [ ] Streamable HTTP transport over the owner's net crates (stateless POST, `MCP-Protocol-Version`, `Mcp-Method`/`Mcp-Name`, `Origin` checks), and the official conformance suite in CI against it (moved from v0.2.0, see above).
- [ ] `pattern` and `patternProperties` (moved from v0.2.0, see above).
- [ ] A `tracing` feature.
- [x] ~~Notifications, progress and cancellation~~ (delivered in v0.2.0).

## v0.9.0 - Hardening
- [ ] `unevaluated*`, `$dynamicRef`, nested `$id` (moved from v0.2.0, see above).
- [x] ~~Conformance against the MCP specification; fuzzing~~ (wire conformance and fuzzing delivered in v0.2.0; the official suite moves with HTTP, see v0.5.0).

## v1.0.0 - Stable
- [ ] Frozen after LexerSketch's ls-mcp ships on it (D18).
