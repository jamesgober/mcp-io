//! Wire conformance: the spec's own example requests, answered, with every
//! message the server sends validated against the official JSON Schema of
//! its revision (`tests/data/mcp-schema/`, see the NOTICE there).
//!
//! This is the check the official conformance suite calls `wire-schema-valid`;
//! the suite itself drives servers over HTTP only, so it cannot run against
//! this crate's stdio transport yet. The validator here is this crate's own
//! (`mcp_io::Schema`); the release verification repeated the check on the
//! stdio server with python-jsonschema, against the 2026-07-28 and
//! 2025-11-25 schemas. 2025-06-18's schema is draft-07; its `$schema` is
//! dropped before compiling, which its keywords allow (they mean the same in
//! 2020-12: no tuple `items`, no `dependencies`), and the compile test below
//! proves every definition compiles.

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::{fs, path::Path, sync::OnceLock, time::Duration};

use common::{Client, code, open_legacy};
use mcp_io::{
    Implementation, Schema, Server, Tier, Tool, ToolResult,
    jsonx::{self, Value, json},
};

const REVISIONS: [&str; 3] = ["2026-07-28", "2025-11-25", "2025-06-18"];

fn spec(revision: &str) -> &'static Value<'static> {
    static CELLS: [OnceLock<Value<'static>>; 3] =
        [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    let at = REVISIONS.iter().position(|r| *r == revision).unwrap();
    CELLS[at].get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/data/mcp-schema/{revision}.json"));
        let mut doc = jsonx::parse(&fs::read_to_string(path).unwrap())
            .unwrap()
            .into_owned();
        // Draft-07 (2025-06-18): read as 2020-12 (see the module docs).
        if doc["$schema"] == "http://json-schema.org/draft-07/schema#" {
            let _dropped = doc.as_object_mut().unwrap().remove("$schema");
        }
        doc
    })
}

/// Where a revision keeps its definitions.
fn defs_key(revision: &str) -> &'static str {
    if revision == "2025-06-18" {
        "definitions"
    } else {
        "$defs"
    }
}

/// Asserts `message` is valid against definition `def` of `revision`.
fn assert_conforms(revision: &str, def: &str, message: &Value<'_>) {
    let mut document = spec(revision).clone();
    let root = document.as_object_mut().unwrap();
    root.push("$ref", format!("#/{}/{def}", defs_key(revision)));
    let schema = Schema::compile(&document).unwrap_or_else(|e| panic!("{revision} {def}: {e}"));
    if let Err(errors) = schema.validate(message) {
        panic!("{revision} {def}: {errors}\n{message}");
    }
}

fn server() -> Server {
    Server::new(Implementation::new("ExampleServer", "1.0.0"))
        .instructions("This server provides weather utilities.")
        .with_workers(0)
        .tool(
            Tool::new("get_weather", Tier::ReadOnly)
                .title("Weather Information Provider")
                .description("Get current weather information for a location")
                .input_schema(json!({
                    "type": "object",
                    "properties": { "location": { "type": "string", "description": "City name or zip code" } },
                    "required": ["location"]
                })),
            |call| {
                call.progress(50.0, Some(100.0), Some("Reticulating splines..."))?;
                let location = call.arguments()["location"].as_str().unwrap_or_default();
                Ok(ToolResult::text(format!("Current weather in {location}:\nTemperature: 72°F")))
            },
        )
        .unwrap()
        .tool(
            Tool::new("get_weather_data", Tier::ReadOnly)
                .output_schema(json!({
                    "type": "object",
                    "properties": { "temperature": { "type": "number" }, "conditions": { "type": "string" } },
                    "required": ["temperature", "conditions"]
                })),
            |_| Ok(ToolResult::structured(json!({ "temperature": 22.5, "conditions": "Partly cloudy" }))),
        )
        .unwrap()
        .tool(Tool::new("deploy", Tier::Publish), |_| Ok(ToolResult::text("deployed")))
        .unwrap()
}

const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"ExampleClient","version":"1.0.0"},"io.modelcontextprotocol/clientCapabilities":{}}"#;

#[test]
fn test_spec_examples_2026_07_28_conform_on_the_wire() {
    let server = server();
    let mut client = Client::new(&server);

    // server/discover (spec: Discovery, "Request").
    let reply = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":"discover-1","method":"server/discover","params":{{{META}}}}}"#
    ));
    assert_eq!(reply["id"], "discover-1");
    assert_conforms("2026-07-28", "DiscoverResultResponse", &reply);

    // tools/list (schema reference, "List tools request").
    let reply = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":"list-tools-example","method":"tools/list","params":{{{META}}}}}"#
    ));
    assert_eq!(reply["result"]["tools"][1]["name"], "get_weather");
    assert_conforms("2026-07-28", "ListToolsResultResponse", &reply);

    // tools/call (schema reference, "Call tool request"), with the progress
    // token example ("Tool call params with progress token").
    let request = r#"{"jsonrpc":"2.0","id":"call-tool-example","method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"ExampleClient","version":"1.0.0"},"io.modelcontextprotocol/clientCapabilities":{},"progressToken":"oivaizmir"},"name":"get_weather","arguments":{"location":"New York"}}}"#;
    client.send(request);
    let progress = client.recv();
    assert_conforms("2026-07-28", "ProgressNotification", &progress);
    let reply = client.recv();
    assert!(common::text(&reply).starts_with("Current weather in New York"));
    assert_conforms("2026-07-28", "CallToolResultResponse", &reply);

    // Structured content with an output schema (Tools, "Output Schema").
    let reply = client.request(&format!(r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{{META},"name":"get_weather_data","arguments":{{"location":"Paris"}}}}}}"#));
    assert_conforms("2026-07-28", "CallToolResultResponse", &reply);

    // A tool execution error (input validation).
    let reply = client.request(&format!(r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{{META},"name":"get_weather","arguments":{{}}}}}}"#));
    assert_eq!(reply["result"]["isError"], true);
    assert_conforms("2026-07-28", "CallToolResultResponse", &reply);

    // A protocol error: unknown tool (Tools, "Error Handling").
    let reply = client.request(&format!(r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{{META},"name":"invalid_tool_name","arguments":{{}}}}}}"#));
    assert_eq!(reply["error"]["message"], "Unknown tool: invalid_tool_name");
    assert_conforms("2026-07-28", "InvalidParamsError", &reply["error"]);
    assert_conforms("2026-07-28", "JSONRPCErrorResponse", &reply);

    // UnsupportedProtocolVersionError (Versioning, the "1900-01-01" example).
    let reply = client.request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1900-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}"#);
    assert_eq!(code(&reply), Some(-32022));
    assert_conforms("2026-07-28", "UnsupportedProtocolVersionError", &reply);

    // An InputRequiredResult, then MissingRequiredClientCapabilityError.
    let reply = client.request(
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{"elicitation":{"form":{}}}},"name":"deploy"}}"#,
    );
    assert_eq!(reply["result"]["resultType"], "input_required");
    assert_conforms("2026-07-28", "CallToolResultResponse", &reply);
    assert_conforms(
        "2026-07-28",
        "ElicitRequest",
        &json!({
            "jsonrpc": "2.0", "id": 0,
            "method": reply["result"]["inputRequests"]["confirm"]["method"].clone(),
            "params": reply["result"]["inputRequests"]["confirm"]["params"].clone()
        }),
    );
    let reply = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{{META},"name":"deploy"}}}}"#
    ));
    assert_eq!(code(&reply), Some(-32021));
    assert_conforms("2026-07-28", "MissingRequiredClientCapabilityError", &reply);

    // subscriptions/listen (Subscriptions, "Opening a Stream"), then a change.
    client.send(&format!(r#"{{"jsonrpc":"2.0","id":"listen-1","method":"subscriptions/listen","params":{{{META},"notifications":{{"toolsListChanged":true,"resourceSubscriptions":["file:///project/config.json"]}}}}}}"#));
    let ack = client.recv();
    assert_eq!(
        ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        "listen-1"
    );
    assert_conforms("2026-07-28", "SubscriptionsAcknowledgedNotification", &ack);
    server
        .tools()
        .add(
            Tool::new("extra", Tier::ReadOnly),
            |_| Ok(ToolResult::new()),
        )
        .unwrap();
    let changed = client.recv();
    assert_conforms("2026-07-28", "ToolListChangedNotification", &changed);

    // Cancellation (Cancellation, "Cancellation Flow"): no response.
    client.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"listen-1","reason":"User requested cancellation"}}"#);
    assert!(client.try_recv(Duration::from_millis(100)).is_none());

    // Malformed input: errors without an id, as the schema allows.
    for bad in ["{", "[]", r#"{"jsonrpc":"2.0","id":1}"#] {
        let reply = client.request(bad);
        assert_conforms("2026-07-28", "JSONRPCErrorResponse", &reply);
    }
    let reply = client.request("{");
    assert_conforms("2026-07-28", "ParseError", &reply["error"]);
    let reply = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"no/such","params":{{{META}}}}}"#
    ));
    assert_conforms("2026-07-28", "MethodNotFoundError", &reply["error"]);

    // And every message of the scenario is a JSON-RPC message of the
    // revision.
    for text in &client.log {
        assert_conforms("2026-07-28", "JSONRPCMessage", &jsonx::parse(text).unwrap());
    }
}

#[test]
fn test_legacy_session_conforms_to_2025_11_25() {
    let server = server().with_workers(2);
    let mut client = Client::new(&server);
    let reply = client.request(&common::initialize(
        1,
        "2025-11-25",
        r#"{"elicitation":{}}"#,
    ));
    assert_conforms("2025-11-25", "InitializeResult", &reply["result"]);
    assert_conforms("2025-11-25", "JSONRPCResultResponse", &reply);
    client.send(common::INITIALIZED);
    let reply = client.request(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#);
    assert_conforms("2025-11-25", "EmptyResult", &reply["result"]);
    let reply = client.request(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
    assert_conforms("2025-11-25", "ListToolsResult", &reply["result"]);
    let reply = client.request(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_weather_data","arguments":{}}}"#);
    assert_conforms("2025-11-25", "CallToolResult", &reply["result"]);
    // A legacy confirmation is a server-sent ElicitRequest.
    client.send(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"deploy"}}"#);
    let elicit = client.recv();
    assert_conforms("2025-11-25", "ElicitRequest", &elicit);
    let id = elicit["id"].to_string();
    client.send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"action":"accept","content":{{"confirm":true}}}}}}"#));
    let reply = client.recv();
    assert_conforms("2025-11-25", "CallToolResult", &reply["result"]);
    server
        .tools()
        .add(
            Tool::new("extra", Tier::ReadOnly),
            |_| Ok(ToolResult::new()),
        )
        .unwrap();
    assert_conforms("2025-11-25", "ToolListChangedNotification", &client.recv());
    for text in &client.log {
        assert_conforms("2025-11-25", "JSONRPCMessage", &jsonx::parse(text).unwrap());
    }
    let _ = open_legacy;
}

#[test]
fn test_the_official_schemas_compile_under_the_supported_subset() {
    // Every revision's schema uses only supported keywords, so the checks
    // above are complete, not partial.
    for revision in REVISIONS {
        let defs = spec(revision)[defs_key(revision)].as_object().unwrap();
        for (name, _) in defs.iter() {
            let mut document = spec(revision).clone();
            document
                .as_object_mut()
                .unwrap()
                .push("$ref", format!("#/{}/{name}", defs_key(revision)));
            assert!(Schema::compile(&document).is_ok(), "{revision} {name}");
        }
    }
}

#[test]
fn test_server_side_close_ends_subscriptions_gracefully() {
    let server = server();
    let mut client = Client::new(&server);
    client.send(&format!(
        r#"{{"jsonrpc":"2.0","id":"listen-1","method":"subscriptions/listen","params":{{{META},"notifications":{{"toolsListChanged":true}}}}}}"#
    ));
    let _ack = client.recv();
    client.send(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"subscriptions/listen","params":{{{META},"notifications":{{}}}}}}"#
    ));
    let _ack = client.recv();
    client.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}"#);
    client.connection.close();
    let closed = client.recv();
    assert_eq!(closed["id"], "listen-1");
    assert_eq!(
        closed["result"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        "listen-1"
    );
    assert_conforms("2026-07-28", "SubscriptionsListenResultResponse", &closed);
    let cancelled = client.recv();
    assert_eq!(cancelled["method"], "notifications/cancelled");
    assert_eq!(cancelled["params"]["requestId"], "listen-1");
    assert_conforms("2026-07-28", "CancelledNotification", &cancelled);
    assert!(
        client.try_recv(Duration::from_millis(100)).is_none(),
        "the cancelled stream gets nothing"
    );
    client.connection.close();
}

/// Tools whose outputs are not objects: an integer output schema, an array
/// result with no schema, and an object one for contrast.
fn output_shapes() -> Server {
    Server::new(Implementation::new("shapes", "1.0.0"))
        .with_workers(0)
        .tool(
            Tool::new("count", Tier::ReadOnly).output_schema(json!({ "type": "integer" })),
            |_| Ok(ToolResult::structured(json!(5))),
        )
        .unwrap()
        .tool(Tool::new("list", Tier::ReadOnly), |_| {
            Ok(ToolResult::text("two items").with_structured(json!(["a", "b"])))
        })
        .unwrap()
        .tool(
            Tool::new("pair", Tier::ReadOnly).output_schema(json!({ "type": "object" })),
            |_| Ok(ToolResult::structured(json!({ "a": 1 }))),
        )
        .unwrap()
}

#[test]
fn test_non_object_outputs_conform_on_every_revision() {
    for revision in REVISIONS {
        let server = output_shapes();
        let mut client = Client::new(&server);
        let modern = revision == "2026-07-28";
        let request = |id: u64, method: &str, params: &str| {
            if modern {
                common::modern(id, method, params)
            } else {
                format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{{params}}}}}"#
                )
            }
        };
        if !modern {
            let reply = client.request(&common::initialize(0, revision, "{}"));
            assert_eq!(reply["result"]["protocolVersion"], revision);
            assert_conforms(revision, "InitializeResult", &reply["result"]);
            client.send(common::INITIALIZED);
        }
        let reply = client.request(&request(1, "tools/list", ""));
        let tools = reply["result"]["tools"].as_array().unwrap();
        let count = tools.iter().find(|t| t["name"] == "count").unwrap();
        // 2026-07-28 allows any output schema; the legacy revisions only
        // object ones, so a legacy session is not shown the integer one.
        assert_eq!(count.get("outputSchema").is_some(), modern, "{revision}");
        let pair = tools.iter().find(|t| t["name"] == "pair").unwrap();
        assert!(pair.get("outputSchema").is_some(), "{revision}");
        if modern {
            assert_conforms(revision, "ListToolsResultResponse", &reply);
        } else {
            assert_conforms(revision, "ListToolsResult", &reply["result"]);
        }
        for (id, tool, structured) in [
            (2, "count", "5"),
            (3, "list", r#"["a","b"]"#),
            (4, "pair", r#"{"a":1}"#),
        ] {
            let reply = client.request(&request(id, "tools/call", &format!(r#""name":"{tool}""#)));
            let result = &reply["result"];
            let sent = result.get("structuredContent").map(ToString::to_string);
            if modern || tool == "pair" {
                assert_eq!(sent.as_deref(), Some(structured), "{revision} {tool}");
            } else {
                assert_eq!(
                    sent, None,
                    "{revision} {tool}: a legacy session gets text only"
                );
                assert!(result["content"].to_string().contains(&if tool == "count" {
                    "5".to_owned()
                } else {
                    "two items".to_owned()
                }));
            }
            if modern {
                assert_conforms(revision, "CallToolResultResponse", &reply);
            } else {
                assert_conforms(revision, "CallToolResult", result);
            }
        }
        for text in &client.log {
            assert_conforms(revision, "JSONRPCMessage", &jsonx::parse(text).unwrap());
        }
    }
}
