# mcp-io &mdash; Engineering Directives

> Engineering standards and the definition of done for this project. Read alongside `REPS.md` (root, authoritative) and `dev/ROADMAP.md` (current phase). If anything here conflicts with `REPS.md`, `REPS.md` wins.
> LexerSketch plan: `C:/Dev/libraries/Rust/lang-collection/_lexersketch/` (README rules of engagement, ARCHITECTURE, ISSUES, ROADMAP).

---

## 0. Philosophy

This library is built and maintained to a production standard and treated as a flagship piece of work. Plan the full path, then build one verified step at a time. "Good enough" is treated as a defect. It is an `-io` crate in the owner's ai-collection: the Model Context Protocol server framework behind LexerSketch's MCP server and every forged language's own MCP server.

---

## 1. What this is

mcp-io owns the MCP lifecycle, capability negotiation, tool/resource/prompt registries, schema validation of tool input, and the permission gate. Tool behaviour belongs to the application (LexerSketch's ls-mcp, or a forged language's MCP server).

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

- No tool runs without passing its permission tier; publish-class tools always require explicit human confirmation and are never auto-approved.
- Tool input is validated against its declared schema before the handler runs.
- Malformed or hostile messages never panic the server; budgets bound every request.
