# mcp-io &mdash; API Reference

> Complete reference for every public item in `mcp-io`; each section has an example.
> **Status: pre-1.0 (0.2.0).** The surface is refined across the 0.x series and
> frozen at `1.0.0`, after LexerSketch's `ls-mcp` has run on it (decision D18).
> See [`../dev/ROADMAP.md`](../dev/ROADMAP.md).

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>

## Table of contents

- [Overview](#overview)
- [Concepts](#concepts)
  - [Two protocol eras](#two-protocol-eras)
  - [Errors on the wire](#errors-on-the-wire)
  - [Threads and cancellation](#threads-and-cancellation)
  - [Standard output](#standard-output)
- [`Server`](#server)
- [`Connection`](#connection)
- [`Lifecycle`](#lifecycle)
- [`Implementation` and `CacheScope`](#implementation-and-cachescope)
- [`Tool` and `Tier`](#tool-and-tier)
- [`Tools`](#tools)
- [`ToolCall`](#toolcall)
- [`ToolResult`, `Content`, `ResourceContents`](#toolresult-content-resourcecontents)
- [`ToolError`](#toolerror)
- [Permissions: `Policy`, `Decision`, `PermissionRequest`, `Ceiling`](#permissions-policy-decision-permissionrequest-ceiling)
- [`AuditRecord` and `Outcome`](#auditrecord-and-outcome)
- [`schema`](#schema)
- [`protocol`](#protocol)
- [`Error` and `RegisterError`](#error-and-registererror)
- [Re-exports](#re-exports)

## Overview

| Item | Kind | Purpose |
|---|---|---|
| [`Server`](#server) | struct | Identity, tools, policy, settings; serves connections. |
| [`Connection`](#connection) | struct | One served connection over any rpc-io `Sink`. |
| [`Lifecycle`](#lifecycle) | enum | A connection's legacy (`initialize`) session state. |
| [`Implementation`](#implementation-and-cachescope), [`CacheScope`](#implementation-and-cachescope) | struct, enum | The server's `serverInfo`; who may cache list results. |
| [`Tool`](#tool-and-tier), [`Tier`](#tool-and-tier), [`JsonSchema`](#tool-and-tier) | struct, enum, trait | A tool's definition; how much it can change; a type's schema. |
| [`Tools`](#tools) | struct | The tool set, changeable while serving. |
| [`ToolCall`](#toolcall) | struct | One call, as its handler sees it. |
| [`ToolResult`](#toolresult-content-resourcecontents), [`Content`](#toolresult-content-resourcecontents), [`ResourceContents`](#toolresult-content-resourcecontents) | struct, enums | What a tool returns. |
| [`ToolError`](#toolerror) | struct | Why a handler produced no result. |
| [`Policy`](#permissions-policy-decision-permissionrequest-ceiling), [`Decision`](#permissions-policy-decision-permissionrequest-ceiling), [`PermissionRequest`](#permissions-policy-decision-permissionrequest-ceiling), [`Ceiling`](#permissions-policy-decision-permissionrequest-ceiling) | trait, enum, structs | The host's say over each call. |
| [`AuditRecord`](#auditrecord-and-outcome), [`Outcome`](#auditrecord-and-outcome) | struct, enum | One call's audit entry. |
| [`Schema`](#schema), `SchemaLimits`, `SchemaError`, `SchemaErrorKind`, `ValidationError`, `Violations` | the `schema` module | JSON Schema 2020-12 (subset) validation. |
| [`protocol`](#protocol) | module | Revisions, reserved `_meta` keys, MCP error codes, `Era`. |
| [`Error`](#error-and-registererror), [`RegisterError`](#error-and-registererror) | enums | Why serving or registering failed. |

## Concepts

### Two protocol eras

MCP 2026-07-28 is stateless: every request names its version in
`params._meta["io.modelcontextprotocol/protocolVersion"]` and the client's
capabilities in `_meta["io.modelcontextprotocol/clientCapabilities"]`, and the
server answers it without relying on earlier requests. Revisions up to
2025-11-25 opened a session with `initialize` instead. A connection serves
both, per request:

- A request with `protocolVersion` is *modern*. The version must be one of
  `protocol::MODERN_PROTOCOL_VERSIONS` (else `-32022`), and
  `clientCapabilities` must be present (else `-32602`). Results carry
  `resultType` and, unless `Server::with_server_info_meta(false)`,
  `_meta["io.modelcontextprotocol/serverInfo"]`.
- A request without it is *legacy*, and needs the connection's legacy
  session: `initialize` must have been answered (else `-32602`, naming the
  missing field). `initialize` itself and `ping` need no session.

| Method | Modern | Legacy |
|---|---|---|
| `server/discover` | yes | `-32601` |
| `tools/list`, `tools/call` | yes | yes |
| `subscriptions/listen` | yes | `-32601` |
| `initialize`, `notifications/initialized` | n/a | yes |
| `ping` | `-32601` (removed in 2026-07-28) | yes |
| anything else | `-32601` | `-32601` |

### Errors on the wire

| Situation | Answer |
|---|---|
| Arguments fail the input schema; the policy denies; the handler returns a `ToolError::new`; the user declines a confirmation | A result with `isError: true` and a message for the model. |
| Unknown tool; missing `name`; `arguments` not an object; params not an object; a malformed `_meta` field; an unknown cursor; an invalid `requestState` | `-32602` |
| A version this server does not serve | `-32022`, `data: {"supported": [...], "requested": ...}`; `supported` holds only the versions a request may name in `_meta` (2026-07-28), since the legacy ones are reached through `initialize` |
| A confirmation needed, and no form elicitation declared | `-32021`, `data: {"requiredCapabilities": {"elicitation": {"form": {}}}}` |
| Handler panic; output not matching the output schema; the call queue full | `-32603` |
| Unknown method | `-32601` |
| Unparseable message; batch; invalid envelope | `-32700` / `-32600`, without an `id` when it cannot be read (rpc-io's MCP preset) |

Nothing in `-32000..=-32019`, and no `-32002`, is ever sent.

### Threads and cancellation

A connection reads and dispatches on the thread that calls
[`Connection::run`](#connection) (or `handle`). Tool calls go to a pool of
worker threads (`Server::with_workers`, default 4), with at most
`Server::with_max_queued_calls` (default 256) running or waiting; more are
refused with `-32603`, as are calls past `with_max_queued_bytes` (64 MiB of
params). So while a tool runs, the connection keeps reading,
and a `notifications/cancelled` for the call sets
[`ToolCall::is_cancelled`](#toolcall); the call is then not answered, and its
progress reports are refused. With `with_workers(0)`, calls run inline on the
reading thread (deterministic, but uncancellable while they run).

When the input ends, `run` handles what is buffered (a last line without
its newline included), waits up to `Server::with_drain_timeout` (10 s) for
running calls to answer, then closes; calls still running are cancelled.

### Standard output

On stdio, standard output carries MCP messages only: one JSON-RPC message per
line, newlines inside JSON text escaped (rpc-io's newline framing). This
crate does not print to standard error either; log there yourself. A
panicking handler is still reported on standard error by Rust's panic hook,
unless the host replaces the hook.

## `Server`

| Method | Description |
|---|---|
| `new(info: Implementation) -> Self` | No tools, the default policy (`Ceiling(Tier::Mutating)`), default settings. |
| `instructions(self, text) -> Self` | MCP `instructions` (in `server/discover` and `initialize`). |
| `tool(self, tool, handler) -> Result<Self, RegisterError>` | Registers a tool; `handler: Fn(&ToolCall) -> Result<ToolResult, ToolError> + Send + Sync + 'static`. |
| `tool_json(self, tool, handler) -> Result<Self, RegisterError>` | Registers a typed tool; `handler: Fn(&ToolCall, A) -> Result<R, ToolError>`, `A: for<'de> FromJson<'de>` (decoded after the schema check), `R: ToJson` (sent as structured content with a text copy). |
| `tools(&self) -> Tools` | The tool set, to change while serving. |
| `policy(self, impl Policy) -> Self` | The permission policy. |
| `on_audit(self, Fn(&AuditRecord) + Send + Sync + 'static) -> Self` | The audit hook, once per `tools/call`. |
| `with_page_size(self, n)` | Tools per `tools/list` page (default 100, at least 1). |
| `with_cache(self, ttl: Duration, CacheScope)` | `ttlMs` and `cacheScope` on `tools/list` and `server/discover` (default 0, public). |
| `with_legacy(self, bool)` | Serve `initialize` clients (default `true`). |
| `with_server_info_meta(self, bool)` | `serverInfo` in modern results' `_meta` (default `true`). |
| `with_list_changed(self, bool)` | Advertise and honour `tools.listChanged` (default `true`). |
| `with_workers(self, n)` | Worker threads per connection (default 4; 0 = inline). |
| `with_max_queued_calls(self, n)` | Calls running or waiting per connection (default 256). |
| `with_max_queued_bytes(self, n)` | The params bytes those calls may hold (default 64 MiB; one larger call is still taken into an empty queue). |
| `with_max_subscriptions(self, n)` | Open `subscriptions/listen` streams per connection (default 16). |
| `with_drain_timeout(self, Duration)` | How long the end of input waits for running calls (default 10 s). |
| `with_confirmation_timeout(self, Duration)` | A `requestState`'s life, and a legacy elicitation's wait (default 10 min). |
| `with_max_confirmed_arguments(self, bytes)` | The largest arguments, as shown, a confirm-tier call may have (default 4 KiB); larger calls are refused, never shown in part. |
| `with_max_pending_confirmations(self, n)` | Outstanding confirmations kept per connection (default 64; the connection's oldest is forgotten). |
| `with_limits(self, rpc_io::Limits)` | Message budgets (default `Limits::mcp()`; batches stay off). |
| `with_schema_limits(self, SchemaLimits)` | Budgets for schemas registered from now on. |
| `connect<K: Sink>(&self, sink) -> Result<Connection<K>, Error>` | A connection sending through `sink`. |
| `serve(&self, input: impl Read, output: impl Write + Send + 'static) -> Result<End, Error>` | One newline-framed connection over a byte stream. |
| `serve_stdio(&self) -> Result<End, Error>` | `serve` on standard input and output. |

**Errors:** `connect` returns `Error::Io` if a worker thread cannot start;
`serve` also returns `Error::Rpc` when reading or writing fails or the input
breaks the framing beyond recovery.

```rust
use std::time::Duration;
use mcp_io::{CacheScope, Implementation, Server, Tier, Tool, ToolResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input: &[u8] = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"#,
        r#""io.modelcontextprotocol/protocolVersion":"2026-07-28","#,
        r#""io.modelcontextprotocol/clientCapabilities":{}}}}"#, "\n",
    ).as_bytes();
    let server = Server::new(Implementation::new("demo", "1.0.0"))
        .with_cache(Duration::from_secs(60), CacheScope::Public)
        .tool(Tool::new("now", Tier::ReadOnly), |_| Ok(ToolResult::text("12:00")))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut connection = server.connect(tx)?;
    connection.run(input)?;
    let reply = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_default();
    assert!(reply.contains(r#""ttlMs":60000"#));
    Ok(())
}
```

## `Connection`

| Method | Description |
|---|---|
| `handle(&mut self, message: &[u8]) -> Result<Flow, Error>` | One message, without its newline. |
| `run(&mut self, input: impl Read) -> Result<End, Error>` | Newline-framed messages until the input ends; drains; closes. |
| `wait_idle(&self, Duration) -> bool` | Waits until no tool call is running or queued. |
| `close(&mut self)` | Answers each open subscription with its graceful-closure result, then sends `notifications/cancelled` for it, then closes (calls running are cancelled). Idempotent. |
| `is_closed(&self)`, `lifecycle(&self)`, `peer(&self)`, `stats(&self)` | State, the legacy session, a sending handle, rpc-io's counters. |

```rust
use std::time::Duration;
use mcp_io::{Implementation, Server, Tier, Tool, ToolResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = Server::new(Implementation::new("demo", "1.0.0"))
        .tool(Tool::new("hi", Tier::ReadOnly), |_| Ok(ToolResult::text("hello")))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut connection = server.connect(tx)?;
    connection.handle(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}},"name":"hi"}}"#)?;
    assert!(connection.wait_idle(Duration::from_secs(10)));
    assert!(rx.try_recv().is_ok_and(|reply| reply.contains(r#""text":"hello""#)));
    connection.close();
    assert!(connection.is_closed());
    Ok(())
}
```

## `Lifecycle`

`#[non_exhaustive]`: `Stateless` (no `initialize` answered), `Initializing {
protocol_version }` (answered; waiting for `notifications/initialized`;
requests are served already), `Operating { protocol_version }`.
`protocol_version()` gives the negotiated revision. `initialize` negotiates
the requested version when it is in `protocol::LEGACY_PROTOCOL_VERSIONS`, and
offers the newest one otherwise. A second `initialize` is `-32600`; one
missing `protocolVersion`, `capabilities`, or `clientInfo` is `-32602`.

```rust
use mcp_io::{Implementation, Lifecycle, Server};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = Server::new(Implementation::new("demo", "1.0.0"));
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut connection = server.connect(tx)?;
    connection.handle(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}"#)?;
    assert!(rx.try_recv().is_ok_and(|r| r.contains(r#""protocolVersion":"2025-11-25""#)));
    assert_eq!(connection.lifecycle().protocol_version(), Some("2025-11-25"));
    assert!(matches!(connection.lifecycle(), Lifecycle::Initializing { .. }));
    Ok(())
}
```

## `Implementation` and `CacheScope`

`Implementation::new(name, version)` with `title`, `description`,
`website_url`; `name()`, `version()`, `to_json()`. `CacheScope`
(`#[non_exhaustive]`): `Public` (default) or `Private`; `as_str()`.

```rust
use mcp_io::{CacheScope, Implementation};

let info = Implementation::new("notes", "2.1.0").title("Notes");
assert_eq!(info.to_json(), r#"{"name":"notes","version":"2.1.0","title":"Notes"}"#);
assert_eq!(CacheScope::Private.as_str(), "private");
```

## `Tool` and `Tier`

`Tier` (`#[non_exhaustive]`, ordered): `ReadOnly < Mutating < Destructive <
Publish`; `name()`, `Display`.

`Tool::new(name, tier)` with `title`, `description`, `input_schema(Value)`,
`output_schema(Value)`, `input_schema_for::<T: JsonSchema>()`,
`output_schema_for::<T: JsonSchema>()`, `idempotent(bool)`, `open_world(bool)`,
`confirmation(question)`; accessors `name`, `tier`, `title_text`,
`description_text`, `input_schema_value`, `output_schema_value`,
`annotations_json`. Registration checks the name (1 to 128 of
`[A-Za-z0-9_.-]`), that an input schema's root is `"type": "object"`, and
compiles both schemas. Without an input schema the tool takes `{}` only
(`{"type":"object","additionalProperties":false}`).

| Tier | `annotations` |
|---|---|
| `ReadOnly` | `readOnlyHint: true` (+ `openWorldHint` if set) |
| `Mutating` | `readOnlyHint: false`, `destructiveHint: false` (+ `idempotentHint`, `openWorldHint` if set) |
| `Destructive` | `readOnlyHint: false`, `destructiveHint: true` (+ as above) |
| `Publish` | `readOnlyHint: false`, `destructiveHint: true`, `openWorldHint: true` (+ `idempotentHint` if set) |

`JsonSchema` is a trait with one function, `json_schema() -> Value<'static>`:
a type's schema, written next to its `FromJson`/`ToJson` impl, so a typed
tool's schema and decoder are defined together.

```rust
use mcp_io::{Tier, Tool, jsonx::json};

let tool = Tool::new("rm", Tier::Destructive)
    .description("Deletes a file")
    .input_schema(json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }))
    .idempotent(true)
    .confirmation("Delete this file?");
assert_eq!(tool.annotations_json(), r#"{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true}"#);
assert!(Tier::Destructive > Tier::Mutating);
```

## `Tools`

`Clone + Send + Sync`, from `Server::tools()`: `add(tool, handler) ->
Result<(), RegisterError>`, `add_json(tool, handler)` (as
`Server::tool_json`), `remove(name) -> bool`, `contains(name)`,
`names()` (sorted), `len()`, `is_empty()`. Every connection sees changes at
once; watchers get `notifications/tools/list_changed` (on a
`subscriptions/listen` stream, tagged with its `subscriptionId`; on an
operating legacy session, plain).

```rust
use mcp_io::{Implementation, Server, Tier, Tool, ToolResult};

fn main() -> Result<(), mcp_io::RegisterError> {
    let server = Server::new(Implementation::new("demo", "1.0.0"));
    let tools = server.tools();
    tools.add(Tool::new("b", Tier::ReadOnly), |_| Ok(ToolResult::new()))?;
    tools.add(Tool::new("a", Tier::ReadOnly), |_| Ok(ToolResult::new()))?;
    assert_eq!(tools.names(), ["a", "b"]);
    assert!(tools.add(Tool::new("a", Tier::ReadOnly), |_| Ok(ToolResult::new())).is_err());
    Ok(())
}
```

## `ToolCall`

What a handler gets: `name()`, `tier()`, `arguments()` (valid against the
input schema; `{}` when none were sent), `id()`, `era()`,
`protocol_version()`, `client_capabilities()` (the request's own, or the
legacy session's), `client_info()` (self-reported; display only),
`is_cancelled()`, `cancel_token()`, `wants_progress()`, and
`progress(progress, total, message) -> Result<(), Error>`.

`decode::<T>()` reads the arguments as any jsonx-lib `FromJson<'a>` type
from the request's own text: a type that borrows (`&'a str`, `Cow<'a, str>`)
borrows from the message with no copy (a string with escapes is unescaped
into a copy). Absent (or `null`) arguments read as `{}`. Arguments that pass the schema
but not `T` are a tool execution error naming the place (`/arguments/...`).

`progress` does nothing when the client sent no `_meta.progressToken`. It
refuses a value that does not increase (`Error::ProgressNotIncreasing`), any
report once the call is cancelled (`Error::Cancelled`), and a value that is
not finite (`Error::Rpc`); nothing is sent then. A `ToolCall` lives only
while its handler runs, so no progress can follow the answer.

```rust
use mcp_io::{Error, Implementation, Server, Tier, Tool, ToolResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = Server::new(Implementation::new("demo", "1.0.0")).with_workers(0).tool(
        Tool::new("index", Tier::Mutating),
        |call| {
            call.progress(1.0, Some(2.0), Some("half"))?;
            assert!(matches!(call.progress(1.0, None, None), Err(Error::ProgressNotIncreasing { .. })));
            Ok(ToolResult::text(format!("indexed for {}", call.protocol_version())))
        },
    )?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    server.connect(tx)?.handle(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},
        "progressToken":"p"},"name":"index"}}"#)?;
    let sent: Vec<String> = rx.try_iter().collect();
    assert!(sent[0].contains(r#""method":"notifications/progress""#));
    assert!(sent[1].contains("indexed for 2026-07-28"));
    Ok(())
}
```

## `ToolResult`, `Content`, `ResourceContents`

`ToolResult`: `new()`, `text(s)`, `error(s)` (`isError: true`),
`structured(Value)`, `encode(&impl ToJson)`, `with(Content)`,
`with_structured(Value)`, `with_error(bool)`; `content()`,
`structured_content()`, `is_error()`. Structured content without other
content gets a text copy when sent. With an output schema, a successful
result must carry structured content that matches it (else `-32603`).

`Content` (`#[non_exhaustive]`): `Text`, `Image`, `Audio` (base64 data and a
MIME type), `ResourceLink`, `Resource` (embedded `ResourceContents::Text` or
`Blob`). Constructors `text`, `image`, `image_bytes`, `audio`,
`audio_bytes`, `resource_link`, `embedded_text`, `embedded_bytes`;
`with_mime_type`, `with_description`; `to_json()`.

```rust
use mcp_io::{Content, ToolResult, jsonx::json};

let result = ToolResult::text("found 1 file")
    .with(Content::resource_link("file:///src/main.rs", "main.rs").with_mime_type("text/x-rust"))
    .with_structured(json!({ "files": ["src/main.rs"] }));
assert_eq!(result.content().len(), 2);
assert_eq!(Content::embedded_bytes("file:///x", b"hi").to_json(), r#"{"type":"resource","resource":{"uri":"file:///x","blob":"aGk="}}"#);
```

## `ToolError`

`new(message)` (a tool execution error: `isError: true`, the message as
text), `protocol(ErrorObject)` (a JSON-RPC error instead),
`missing_capability(&Value)` (MCP's `-32021`). `From<E: std::error::Error>`
makes `?` work in handlers; `Display` shows the message. Messages are sent to
the client: keep them actionable and free of secrets.

```rust
use mcp_io::{ToolError, jsonx::json};

fn open(path: &str) -> Result<String, ToolError> {
    Ok(std::fs::read_to_string(path)?) // an io::Error becomes a tool execution error
}
assert!(open("/no/such/file").is_err());
assert!(ToolError::missing_capability(&json!({ "elicitation": {} })).to_string().contains("-32021"));
```

## Permissions: `Policy`, `Decision`, `PermissionRequest`, `Ceiling`

Every call passes the gate after validation: the policy's `Decision`
(`#[non_exhaustive]`: `Allow`, `Deny(reason)`, `Confirm`; `Decision::deny`)
is applied, except that a `Publish` tool's `Allow` becomes `Confirm`. A
panicking policy denies.

`Confirm` asks the user through the client. On 2026-07-28 the call is
answered with `resultType: "input_required"`, an `inputRequests.confirm`
holding a form `elicitation/create` (the tool's `confirmation` question, the
tool, its tier, and the arguments in full: canonical JSON with every
non-ASCII character escaped), and a `requestState` bound to exactly that
text. The user is never shown part of the arguments: a call whose shown
arguments exceed `Server::with_max_confirmed_arguments` (4 KiB) is refused
with a tool error instead. The server keeps only a keyed digest of the arguments, so a
pending confirmation costs the same whatever their size; each connection may
hold `with_max_pending_confirmations` (64) of them, its oldest forgotten
first. The client retries the call (a new id, the same name and
arguments) with that `requestState` and `inputResponses.confirm =
{"action": "accept", "content": {"confirm": true}}`; the tool then runs.
The policy is not consulted for the retry: it decided the call when the
confirmation was asked for, so a policy that counts calls counts it once.
`decline`, `cancel`, or `confirm: false` is a tool error; a retry without an
answer is asked again. A `requestState` names a server-side entry bound to
the tool's registration (a tool removed and registered again does not
inherit it) and the exact (canonical) arguments, valid for the confirmation
timeout and consumed when presented: a replayed, altered, expired, or
mismatched one is `-32602`. Without form elicitation in the request's
capabilities: `-32021`. On a legacy session the server sends
`elicitation/create` itself and waits for the answer (with worker threads
only); if the client cancels the `tools/call` meanwhile, the wait ends, the
elicitation is withdrawn, and nothing is sent for the call. A client that
declared no elicitation gets a tool error.

`PermissionRequest`: `tool()`, `tier()`, `arguments()`, `client_info()`
(self-reported; never for security), `era()`. `Ceiling(tier)` allows tiers up
to `tier` and confirms above (default `Ceiling(Tier::Mutating)`). A closure
`Fn(&PermissionRequest) -> Decision` is a `Policy`.

The gate guarantees that a denied call never runs, that a confirm-tier call
runs only with a confirmation the server issued for that tool and those
arguments, and that each confirmation runs one call. It cannot prove a
person answered: the client relays the answer. It is not a sandbox.

```rust
use mcp_io::{Decision, Implementation, PermissionRequest, Server, Tier, Tool, ToolResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = Server::new(Implementation::new("demo", "1.0.0"))
        .with_workers(0)
        .policy(|request: &PermissionRequest<'_>| match request.tier() {
            Tier::ReadOnly => Decision::Allow,
            _ => Decision::Confirm,
        })
        .tool(Tool::new("ship", Tier::Publish), |_| Ok(ToolResult::text("shipped")))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut connection = server.connect(tx)?;
    connection.handle(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{"elicitation":{}}},"name":"ship"}}"#)?;
    let reply = mcp_io::jsonx::parse(&rx.try_recv()?)?.into_owned();
    assert_eq!(reply["result"]["resultType"], "input_required");
    let state = reply["result"]["requestState"].as_str().unwrap_or_default().to_owned();
    let retry = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"_meta":{{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{{"elicitation":{{}}}}}},"name":"ship",
        "requestState":"{state}","inputResponses":{{"confirm":{{"action":"accept","content":{{"confirm":true}}}}}}}}}}"#
    );
    connection.handle(retry.as_bytes())?;
    assert!(rx.try_recv()?.contains("shipped"));
    Ok(())
}
```

## `AuditRecord` and `Outcome`

The hook from `Server::on_audit` gets one `AuditRecord` per `tools/call`
request, refused ones included: `tool()`, `tier()`, `era()` (each an
`Option`: `None` for a request refused before it named a registered tool or
an era), `id()`, `client()` (self-reported name, clipped to 128 bytes),
`outcome()`, `elapsed()`. Never the arguments or the result. `Outcome`
(`#[non_exhaustive]`): `Refused` (a JSON-RPC error: unknown tool, malformed
request, unsupported version, no session, full queue), `InvalidArguments`, `Denied`,
`ConfirmationRequested`, `NotConfirmed`, `Completed { is_error }`, `Failed`,
`Cancelled`; `ran()` says whether the handler ran. A panicking hook is
ignored.

```rust
use std::sync::{Arc, Mutex};
use mcp_io::{Implementation, Outcome, Server, Tier, Tool, ToolResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let outcomes: Arc<Mutex<Vec<Outcome>>> = Arc::default();
    let seen = Arc::clone(&outcomes);
    let server = Server::new(Implementation::new("demo", "1.0.0"))
        .with_workers(0)
        .on_audit(move |record| seen.lock().map(|mut o| o.push(record.outcome())).unwrap_or(()))
        .tool(Tool::new("t", Tier::ReadOnly), |_| Ok(ToolResult::text("ok")))?;
    let (tx, _rx) = std::sync::mpsc::channel::<String>();
    server.connect(tx)?.handle(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"t"}}"#)?;
    assert_eq!(*outcomes.lock().unwrap_or_else(|e| e.into_inner()), [Outcome::Completed { is_error: false }]);
    Ok(())
}
```

## `schema`

`Schema::compile(&Value)`, `compile_with(&Value, SchemaLimits)`,
`validate(&Value) -> Result<(), Violations>`, `is_valid`, `limits`. `Send +
Sync`.

**Supported (2020-12):** `type`, `enum`, `const`; `minimum`, `maximum`,
`exclusiveMinimum`, `exclusiveMaximum`, `multipleOf` (exact decimal
arithmetic; a divisor of at most 19 significant digits); `minLength`,
`maxLength` (code points); `prefixItems`, `items`, `contains`, `minContains`,
`maxContains`, `minItems`, `maxItems`, `uniqueItems`; `properties`,
`additionalProperties`, `required`, `propertyNames`, `minProperties`,
`maxProperties`, `dependentRequired`, `dependentSchemas`; `allOf`, `anyOf`,
`oneOf`, `not`, `if`/`then`/`else`; `$ref` to `#`, `#/pointer` (anywhere in
the document) and `#anchor`; `$defs`, `$anchor`, `$schema` (2020-12 only),
`$id` at the root. Annotations (`title`, `description`, `default`,
`examples`, `format`, ...) and unknown keywords never fail.

**Refused at compile time:** `pattern`, `patternProperties`,
`unevaluatedProperties`, `unevaluatedItems`, `$dynamicRef`,
`$dynamicAnchor`, `$recursiveRef`, `$recursiveAnchor`, `$vocabulary`,
draft-07's `dependencies` and `additionalItems`, `items` as an array, any
other `$schema`, `$id` in a subschema, and any `$ref` that is not a fragment
of the document (never fetched).

Numbers compare by exact decimal value (`1` equals `1.0`). A number whose
exponent is beyond ±2^52 cannot be compared exactly, so an instance holding
one is refused (keyword `number`) wherever its value matters (`enum`,
`const`, `uniqueItems`, bounds, `multipleOf`, `integer`).

`SchemaLimits` (`#[non_exhaustive]`, `Copy`, `const fn new()`, `with_*`):
`max_depth` 64, `max_nodes` 4,096, `max_steps` 4,194,304, `max_validation_depth`
512 (one per instance level and per `$ref` followed), `max_errors` 16. Running out during validation refuses the instance
(`Violations::budget_exceeded`).

`SchemaError`: `pointer()` (into the schema), `kind()`; `SchemaErrorKind`
(`#[non_exhaustive]`): `NotASchema`, `InvalidKeyword`, `Unsupported`,
`UnsupportedDialect`, `DuplicateAnchor`, `NonLocalRef`, `UnresolvedRef`,
`ReferenceCycle` (a `$ref` cycle that never descends into a property or an
item, such as `{"$ref": "#"}`, which validation could never finish),
`TooDeep`, `TooLarge`. `Violations`: `errors()`, `truncated()`,
`budget_exceeded()`; `ValidationError`: `instance_path()` (JSON Pointer),
`keyword()`, `message()` (never quotes instance values). Paths and messages
name properties, and a property name from the instance is clipped to 64
bytes, marked `…`; a path is clipped to 512 bytes. A `tools/call` report
sent to the client is clipped to 8 KiB.

```rust
use mcp_io::{Schema, SchemaErrorKind, jsonx::{json, parse}};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let price = Schema::compile(&parse(r#"{"type": "number", "multipleOf": 0.01, "exclusiveMinimum": 0}"#)?)?;
    assert!(price.is_valid(&parse("19.99")?));
    assert!(!price.is_valid(&parse("19.999")?));
    let refused = Schema::compile(&json!({ "type": "string", "pattern": "^a" })).unwrap_err();
    assert!(matches!(refused.kind(), SchemaErrorKind::Unsupported { .. }));
    Ok(())
}
```

## `protocol`

Constants: `LATEST_PROTOCOL_VERSION` (`"2026-07-28"`),
`MODERN_PROTOCOL_VERSIONS`, `LEGACY_PROTOCOL_VERSIONS` (`["2025-11-25",
"2025-06-18"]`), `UNSUPPORTED_PROTOCOL_VERSION` (`-32022`),
`MISSING_REQUIRED_CLIENT_CAPABILITY` (`-32021`), `HEADER_MISMATCH`
(`-32020`, HTTP only), and `meta::{PROTOCOL_VERSION, CLIENT_CAPABILITIES,
CLIENT_INFO, LOG_LEVEL, SERVER_INFO, SUBSCRIPTION_ID, PROGRESS_TOKEN}`.
`Era` (`#[non_exhaustive]`: `Modern`, `Legacy`). `unsupported_version(&str)`
and `missing_capability(&Value)` build those errors.

```rust
use mcp_io::protocol;

assert_eq!(protocol::LATEST_PROTOCOL_VERSION, protocol::MODERN_PROTOCOL_VERSIONS[0]);
let error = protocol::unsupported_version("2099-01-01");
assert_eq!(error.code(), protocol::UNSUPPORTED_PROTOCOL_VERSION);
```

## `Error` and `RegisterError`

`Error` (`#[non_exhaustive]`): `Rpc(rpc_io::Error)` (the connection failed
or is closed), `Io(io::Error)` (a thread could not start), `Cancelled`
(nothing more may be sent for the call), `ProgressNotIncreasing { last, next
}`. `RegisterError` (`#[non_exhaustive]`): `InvalidName { name, reason }`,
`Duplicate { name }`, `InputSchemaNotObject { tool }`, `Schema { tool,
which, error }`. Both implement `std::error::Error`.

```rust
use mcp_io::{Implementation, RegisterError, Server, Tier, Tool, ToolResult, jsonx::json};

let err = Server::new(Implementation::new("demo", "1.0.0"))
    .tool(Tool::new("t", Tier::ReadOnly).input_schema(json!({ "type": "array" })), |_| Ok(ToolResult::new()))
    .err();
assert!(matches!(err, Some(RegisterError::InputSchemaNotObject { .. })));
```

## Re-exports

`mcp_io::jsonx` is jsonx-lib (`json!`, `Value`, `ToJson`, `parse`, ...) and
`mcp_io::rpc_io` is rpc-io (`Sink`, `Limits`, `ErrorObject`, `Peer`, ...),
at the versions this crate is built with, so a server needs no direct
dependency on either.

```rust
use mcp_io::{jsonx::json, rpc_io::ErrorObject};

let value = json!({ "ok": true });
assert_eq!(value["ok"], true);
assert_eq!(ErrorObject::invalid_params("x").code().0, -32602);
```
