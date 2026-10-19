//! A minimal MCP server over stdio, with a tool per permission tier.
//!
//! Run it as an MCP client's subprocess (the client writes requests to its
//! standard input and reads answers from its standard output):
//!
//! ```text
//! cargo run --example stdio_server
//! ```
//!
//! Or by hand, typing one JSON-RPC message per line:
//!
//! ```text
//! {"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}
//! ```
//!
//! Standard output carries MCP messages only; this example logs to standard
//! error, as the stdio transport asks.

use std::sync::{Arc, Mutex, PoisonError};

use mcp_io::{
    Implementation, Server, Tier, Tool, ToolError, ToolResult,
    jsonx::{Value, json},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let notes: Arc<Mutex<Vec<String>>> = Arc::default();
    let (adding, listing, clearing) = (Arc::clone(&notes), Arc::clone(&notes), notes);

    let server = Server::new(Implementation::new(
        "notes-example",
        env!("CARGO_PKG_VERSION"),
    ))
    .instructions(
        "Keep short notes. `add` does arithmetic; `notes_add` and `notes_list` keep notes.",
    )
    .on_audit(|record| {
        eprintln!(
            "[audit] {} ({}) by {:?}: {}",
            record.tool().unwrap_or("-"),
            record.tier().map_or("-", |tier| tier.name()),
            record.client(),
            record.outcome()
        )
    })
    .tool(
        Tool::new("add", Tier::ReadOnly)
            .title("Add")
            .description("Adds two integers and returns the sum.")
            .input_schema(json!({
                "type": "object",
                "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                "required": ["a", "b"],
                "additionalProperties": false
            }))
            .output_schema(json!({
                "type": "object",
                "properties": { "sum": { "type": "integer" } },
                "required": ["sum"]
            })),
        |call| {
            let (a, b) = (int(call.arguments(), "a")?, int(call.arguments(), "b")?);
            let sum = a
                .checked_add(b)
                .ok_or_else(|| ToolError::new("the sum overflows 64 bits"))?;
            Ok(ToolResult::structured(json!({ "sum": sum })))
        },
    )?
    .tool(
        Tool::new("notes_add", Tier::Mutating)
            .description("Stores a note.")
            .input_schema(json!({
                "type": "object",
                "properties": { "text": { "type": "string", "minLength": 1, "maxLength": 1000 } },
                "required": ["text"],
                "additionalProperties": false
            })),
        move |call| {
            let text = call.arguments()["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let mut notes = adding.lock().unwrap_or_else(PoisonError::into_inner);
            notes.push(text);
            Ok(ToolResult::text(format!("stored note {}", notes.len())))
        },
    )?
    .tool(
        Tool::new("notes_list", Tier::ReadOnly).description("Lists the stored notes."),
        move |_| {
            let notes = listing.lock().unwrap_or_else(PoisonError::into_inner);
            Ok(ToolResult::text(if notes.is_empty() {
                "no notes".to_owned()
            } else {
                notes.join("\n")
            }))
        },
    )?
    .tool(
        Tool::new("notes_clear", Tier::Destructive)
            .description("Deletes every note. Asks the user first.")
            .confirmation("Delete every stored note?"),
        move |_| {
            clearing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
            Ok(ToolResult::text("all notes deleted"))
        },
    )?;

    eprintln!("notes-example: serving MCP on stdio");
    server.serve_stdio()?;
    eprintln!("notes-example: standard input closed, exiting");
    Ok(())
}

/// An integer argument. The input schema has checked it is an integer; this
/// refuses one too large for 64 bits (JSON integers have no limit).
fn int(arguments: &Value<'_>, name: &str) -> Result<i64, ToolError> {
    arguments[name]
        .as_i64()
        .ok_or_else(|| ToolError::new(format!("`{name}` must fit in a 64-bit signed integer")))
}
