# mcp-io &mdash; Engineering Directives

> Engineering standards and the definition of done for this project. Read alongside `REPS.md` (root, authoritative) and `dev/ROADMAP.md` (current phase). If anything here conflicts with `REPS.md`, `REPS.md` wins.
> LexerSketch plan: `C:/Dev/libraries/Rust/lang-collection/_lexersketch/` (README rules of engagement, ARCHITECTURE, ISSUES, ROADMAP).

---

## 0. Philosophy

This library is built and maintained to a production standard and treated as a flagship piece of work. Plan the full path, then build one verified step at a time. "Good enough" is treated as a defect. It is an `-io` crate in the owner's ai-collection: the Model Context Protocol server framework behind LexerSketch's MCP server and every forged language's own MCP server.

---

## 1. What this is

mcp-io owns MCP's protocol layer for servers: per-request versioning (MCP 2026-07-28, stateless) and the legacy `initialize` lifecycle (2025-11-25 and 2025-06-18) on the same connection, discovery and capabilities, the tool registry (resource and prompt registries later), JSON Schema validation of tool input and output, and the permission gate. Tool behaviour belongs to the application (LexerSketch's ls-mcp, or a forged language's MCP server). JSON-RPC itself (framing, correlation, cancellation, budgets, output queues) belongs to rpc-io.

ARCHITECTURE section 14: LexerSketch exposes its engine to AI clients, and every language it forges ships an MCP server; one framework serves both.

---

## 2. Engineering law (non-negotiable)

- **Ours, not rented.** No third-party runtime dependency. First-party `-lang` crates only, wired when first used. Dev-only test tooling is allowed.
- **Performance.** Peak is the baseline: allocation-free hot paths where feasible, dense ids instead of maps, no "faster" claim without `criterion` numbers at realistic scale (100k+ items).
- **Correctness.** Every invariant in section 4 is covered by property tests, cross-checked against a simple reference implementation wherever one can be written.
- **Hostile-input hardening.** Any input this crate accepts may be adversarial: explicit budgets (depth, size, steps), iterative algorithms, no quadratic paths, no reachable panics.
- **Architecture.** SOLID, KISS, YAGNI; one responsibility; a Tier-1 one-call entry point headlines the docs.
- **Cross-platform.** Linux, macOS, and Windows are first-class, verified by CI on stable and MSRV 1.85.
- **Error handling.** Every fallible path returns `Result`/`Option` per a documented contract; errors are `#[non_exhaustive]` and actionable.
- **Production-ready.** `#![forbid(unsafe_code)]` and `#![deny(missing_docs)]` from the first commit; no `unwrap`/`expect`/`todo!`/`dbg!`/printing in library code; every public item has rustdoc with a runnable example; every claim in the docs maps to a test.

---

## 3. Definition of done

1. Compiles clean on Linux/macOS/Windows, stable and MSRV 1.85.
2. `fmt`, `clippy -D warnings` (all targets, all/no-default features), `test --all-features`, `cargo doc -D warnings` clean.
3. `cargo audit` + `cargo deny check` pass.
4. No `unwrap`/`expect`/`todo!`/`dbg!` in shipping code.
5. A Tier-1 API exists and headlines the docs.
6. Property tests cover every section-4 invariant; a fuzz target exists for every decoder or parser.
7. Hot-path changes carry benchmarks; no regression over 5%.
8. `README.md`, `docs/API.md`, and `CHANGELOG.md` updated; the matching `docs/release/vX.Y.Z.md` written before the tag.
9. v1.0.0 only after a real consumer has exercised the API end to end (LexerSketch decision D18).

---

## 4. Project-specific invariants

- No tool runs without passing the permission gate. A `Publish` tool never runs without a confirmation obtained through the client's elicitation, issued by this server for that exact tool and those exact arguments, and used once; no policy can waive it. The server cannot prove a human answered (the client relays it), and the docs say so.
- Tool input is validated against its declared schema before the gate and the handler; a schema outside the supported subset is refused at registration, never half-applied.
- Structured output is validated against the tool's output schema before it is sent.
- Every request gets exactly one answer, or none when the client cancelled it; nothing is sent for a cancelled request.
- Malformed or hostile messages never panic the server; budgets bound every request and every validation.

---

## 5. Recorded decisions and waivers

Decisions taken for v0.2.0 against the 2026-10-14 baseline audit (`_audits/2026-10-14/mcp-io.md`, outside the repo).

- **Protocol target (MCP-ARC-01, MCP-ARC-11): dual-era.** MCP 2026-07-28 removed the `initialize` handshake. The modern revision is the primary target: a request carrying `_meta["io.modelcontextprotocol/protocolVersion"]` is served statelessly. Because many hosts still speak 2025-11-25, a connection that opens with `initialize` is served with the negotiated legacy revision (2025-11-25 or 2025-06-18), as the spec's "dual-era" server is allowed to. 2025-03-26 and older are not served: they require JSON-RPC batches. `Server::with_legacy(false)` makes a modern-only server, which answers `initialize` with `-32022` naming its versions (spec SHOULD).
- **rpc-io preset (MCP-ARC-02).** rpc-io 0.4's `Config::mcp()` is the 2026-07-28 preset (no gate, batches off, MCP cancellation, no `null` ids, `-32603` for overload). mcp-io uses it with the gate off and keeps the legacy lifecycle itself, per connection.
- **JSON Schema (MCP-ARC-04): a first-party validator for a documented subset, always on.** What MCP requires of a server: publish an `inputSchema` (root `"type": "object"`) and an optional `outputSchema` for every tool; "Validate all tool inputs"; return structured content that conforms to the output schema; support 2020-12 for schemas without `$schema`, and answer unsupported dialects with an error; never dereference network `$ref`s; bound composition. No first-party validator existed, and "ours, not rented" rules out a third-party one, so `mcp_io::schema` implements the 2020-12 assertion and applicator vocabulary except `pattern`/`patternProperties` (no first-party regex engine yet), `unevaluated*`, and dynamic references. Every refused keyword, dialect, or non-local `$ref` fails registration with a `SchemaError`, so a schema is never half-applied, and the docs never claim full 2020-12. Validation is not behind a feature: the crate's invariant is that no handler runs on unvalidated input. It is checked against the official JSON Schema Test Suite (draft 2020-12): no wrong answer on any schema it accepts.
- **Permission tiers (MCP-ARC-03): annotations, a host policy, and confirmation through MRTR elicitation.** Tiers (`ReadOnly < Mutating < Destructive < Publish`) map to tool annotations (untrusted hints for the client) and drive the gate: the host's `Policy` allows, denies, or asks; `Publish` always asks. Asking uses the spec's mechanism: an `InputRequiredResult` with a form `elicitation/create` and a `requestState` (2026-07-28), or a server-sent `elicitation/create` (legacy sessions). A client without form elicitation gets `-32021`, and the tool does not run. The `requestState` is a 128-bit random token naming a server-side entry (the tool's registration, a keyed digest of the canonical arguments, an expiry), consumed on first presentation and capped per connection. Its integrity needs no MAC because nothing in it is trusted: a tampered token names nothing. On stdio there is no authenticated principal to bind. Single use is enforced server-side, as the spec requires of one-time redemptions. The README promise "dangerous actions always require a human" was replaced with what the gate can and cannot guarantee.
- **Logging, Roots, Sampling: not adopted.** 2026-07-28 deprecated them ("new implementations SHOULD NOT adopt"). The server never sends `notifications/message`, declares no `logging` capability, and does not serve `logging/setLevel`. A request's `io.modelcontextprotocol/logLevel` is still validated (`-32602` for an unknown level, spec SHOULD).
- **No async runtime; threads (REPS Concurrency, MCP-STD-05).** REPS says "MUST use tokio"; DIRECTIVES says "no third-party runtime dependency", and REPS wins a conflict, so this is a recorded waiver, as in rpc-io: the crate is synchronous over rpc-io's endpoint, with a bounded worker pool per connection (no unbounded spawning; a full queue is refused with `-32603`), and `std` locks are never held while user code runs.
- **`thiserror`, `tracing` (REPS Error Handling, Observability): waived for now.** Error types are hand-written, `#[non_exhaustive]`, and documented per variant. Observability is the audit hook (one record per tool call, never arguments) and rpc-io's `Stats` through `Connection::stats`. A `tracing` feature is deferred (ROADMAP).
- **`#![deny(warnings)]` in source**: kept, as REPS requires. **`clippy::undocumented_unsafe_functions`** (REPS list) is not a clippy lint; `clippy::missing_safety_doc` and `clippy::undocumented_unsafe_blocks` are denied instead, and `forbid(unsafe_code)` makes them moot (MCP-STD-01).
- **`rust-toolchain.toml` (MCP-STD-02): waived.** A library is checked on stable and on its MSRV (1.85), in CI and locally; pinning one toolchain in the repository would stop either from being the default. `Cargo.lock` is not committed: a library's dependents resolve their own.
- **`dev/DIRECTIVES.md` and `dev/ROADMAP.md` are committed (MCP-STD-03).** They are the owner's engineering contract for the crate, not working notes, and they stay out of the published package (`Cargo.toml` `include`).
- **Fixed clip lengths**: 128 bytes for client text quoted in errors (an unknown tool's name, a requested version). These are security caps on client-controlled text, not tuning knobs. A confirmation never clips: it shows the arguments in full or refuses the call (`with_max_confirmed_arguments`, default 4 KiB), because a clipped question let a padded call hide its decisive argument (release verification MCP2-002).
- **Threads per connection**: with the default 4 workers, a connection costs rpc-io's writer thread plus 4 worker threads, each with an 8 MiB stack (address space reserved, committed as used), sized for the validator's nesting cap (512 applications) with room for handlers; `with_workers(0)` runs calls inline on the reading thread, which a 1 MiB stack also fits.
