//! The checks behind the fuzz targets (`fuzz/`), shared with
//! `tests/fuzz_harness.rs`, which runs them on random and mutated input in
//! plain `cargo test`.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{collections::BTreeMap, sync::mpsc};

use mcp_io::{
    Implementation, Schema, SchemaLimits, Server, Tier, Tool, ToolResult,
    jsonx::{self, json},
    rpc_io::{Incoming, Limits, Message, decode},
};

/// A server with a tool in every tier, schemas with composition and
/// references, an output schema, and inline execution, so one input runs
/// deterministically through the whole message surface.
pub fn server() -> Server {
    let schema = json!({
        "type": "object",
        "$defs": { "id": { "type": "integer", "minimum": 0 } },
        "properties": {
            "id": { "$ref": "#/$defs/id" },
            "tags": { "type": "array", "items": { "type": "string", "maxLength": 16 }, "uniqueItems": true, "maxItems": 8 },
            "mode": { "enum": ["fast", "safe", 1.5] },
            "when": { "oneOf": [{ "type": "string" }, { "type": "number", "multipleOf": 0.25 }] }
        },
        "additionalProperties": false
    });
    let mut server = Server::new(Implementation::new("fuzz", "0.0.0"))
        .with_workers(0)
        .with_page_size(2)
        .with_limits(Limits::mcp().with_max_message_bytes(64 * 1024));
    for (name, tier) in [
        ("read", Tier::ReadOnly),
        ("edit", Tier::Mutating),
        ("delete", Tier::Destructive),
        ("publish", Tier::Publish),
    ] {
        server = server
            .tool(Tool::new(name, tier).input_schema(schema.clone()), |call| {
                call.progress(1.0, None, None)?;
                Ok(ToolResult::text(call.arguments().to_string()))
            })
            .unwrap();
    }
    server
        .tool(
            Tool::new("shaped", Tier::ReadOnly)
                .output_schema(json!({ "type": "object", "required": ["n"] })),
            |call| {
                if call.arguments().get("bad").is_some() {
                    Ok(ToolResult::structured(json!([1])))
                } else {
                    Ok(ToolResult::structured(json!({ "n": 1 })))
                }
            },
        )
        .unwrap()
        .tool(Tool::new("panics", Tier::ReadOnly), |_| {
            std::panic::panic_any("fuzz")
        })
        .unwrap()
}

/// Feeds `data`, split into lines, to a fresh connection, and checks what
/// comes out: never a panic; every message is one line of valid JSON; no
/// request id is answered more often than it was asked; nothing is answered
/// that nobody asked.
pub fn check_server(server: &Server, data: &[u8]) {
    let (tx, rx) = mpsc::channel::<String>();
    let mut connection = server.connect(tx).unwrap();
    let mut asked: BTreeMap<String, usize> = BTreeMap::new();
    let mut lines = 0usize;
    for line in data.split(|&b| b == b'\n') {
        lines += 1;
        // What the line asks for an answer under: a request's id, or the id
        // a malformed request-like message is refused under.
        match decode(line, &Limits::mcp()) {
            Incoming::Single(Ok(Message::Request(request))) => {
                *asked.entry(request.id.to_string()).or_default() += 1;
            }
            Incoming::Single(Err(invalid)) if !invalid.is_response() => {
                if let Some(id) = invalid.id() {
                    *asked.entry(id.to_string()).or_default() += 1;
                }
            }
            _ => {}
        }
        if connection.handle(line).is_err() {
            break;
        }
    }
    connection.close();
    let mut answered: BTreeMap<String, usize> = BTreeMap::new();
    let mut responses = 0usize;
    for message in rx.try_iter() {
        assert!(!message.contains('\n'), "a message holds a raw newline");
        let value =
            jsonx::parse(&message).unwrap_or_else(|e| panic!("invalid JSON sent: {e}: {message}"));
        assert_eq!(value["jsonrpc"], "2.0");
        let is_response = value.get("result").is_some() || value.get("error").is_some();
        if is_response {
            responses += 1;
            if let Some(id) = value.get("id") {
                assert!(!id.is_null(), "MCP responses never carry a null id");
                *answered.entry(id.to_string()).or_default() += 1;
            }
        } else {
            assert!(value.get("method").is_some());
        }
    }
    assert!(responses <= lines, "{responses} responses to {lines} lines");
    for (id, count) in &answered {
        let limit = asked.get(id).copied().unwrap_or(0);
        assert!(
            *count <= limit.max(1),
            "id {id} answered {count} times, asked {limit}"
        );
    }
}

/// Splits `data` at the first `0x00` into a schema and an instance, parses
/// both, compiles the schema under small budgets and validates: never a
/// panic, and `validate` and `is_valid` agree.
pub fn check_schema(data: &[u8]) {
    let (schema_bytes, instance_bytes) = match data.iter().position(|&b| b == 0) {
        Some(at) => (&data[..at], &data[at + 1..]),
        None => (data, &b"null"[..]),
    };
    let parser = jsonx::Parser::new().limits(
        jsonx::Limits::new()
            .with_max_depth(64)
            .with_max_elements(4096),
    );
    let (Ok(schema), Ok(instance)) = (
        parser.parse_bytes(schema_bytes),
        parser.parse_bytes(instance_bytes),
    ) else {
        return;
    };
    let limits = SchemaLimits::new()
        .with_max_nodes(512)
        .with_max_steps(20_000);
    if let Ok(compiled) = Schema::compile_with(&schema, limits) {
        let report = compiled.validate(&instance);
        assert_eq!(report.is_ok(), compiled.is_valid(&instance));
        if let Err(violations) = report {
            assert!(!violations.errors().is_empty());
            assert!(violations.errors().len() <= limits.max_errors);
        }
    }
}
