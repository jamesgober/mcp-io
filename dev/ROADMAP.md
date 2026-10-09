# mcp-io - Roadmap

> Path from scaffold to a stable 1.0. Hard parts are front-loaded; each phase has hard exit criteria.
> Master plan: C:/Dev/libraries/Rust/lang-collection/_lexersketch/ROADMAP.md and NEW-LIBS.md
>
> **Anti-deferral rule:** no listed hard task moves to a later phase unless this file records the move and the reason.

## v0.1.0 - Scaffold (DONE)
Compiles, CI green, structure correct, no domain logic.
- [x] Manifest, README, CHANGELOG, REPS, dual license, CI, deny, clippy, rustfmt, DIRECTIVES, ROADMAP.

## v0.2.0 - Foundation
- [ ] Lifecycle and capabilities, tool registry with schemas, permission tiers, stdio transport via rpc-io.

## v0.5.0 - Implementation
- [ ] Resources, prompts, notifications, HTTP transport, progress and cancellation.

## v0.9.0 - Hardening
- [ ] Conformance against the MCP specification; fuzzing.

## v1.0.0 - Stable
- [ ] Frozen after LexerSketch's ls-mcp ships on it (D18).
