//! `tools/list` and `tools/call`: pagination, validation, tool errors versus
//! protocol errors, structured output, cancellation, progress, and
//! list-changed notifications.

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use common::{Client, call, code, modern, open_legacy, text};
use mcp_io::{
    CacheScope, Content, Error, Implementation, Server, Tier, Tool, ToolError, ToolResult,
    jsonx::json, rpc_io::ErrorObject,
};

fn base() -> Server {
    Server::new(Implementation::new("tools-test", "1.0.0"))
}

fn adder() -> Tool {
    Tool::new("add", Tier::ReadOnly)
        .description("Adds two integers")
        .input_schema(json!({
            "type": "object",
            "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
            "required": ["a", "b"],
            "additionalProperties": false
        }))
}

fn add(call: &mcp_io::ToolCall<'_>) -> Result<ToolResult, ToolError> {
    let a = call.arguments()["a"].as_i64().unwrap_or(0);
    let b = call.arguments()["b"].as_i64().unwrap_or(0);
    match a.checked_add(b) {
        Some(sum) => Ok(ToolResult::text(sum.to_string())),
        None => Err(ToolError::new(
            "the sum overflows a 64-bit integer; use smaller numbers",
        )),
    }
}

#[test]
fn test_list_is_sorted_paginated_and_cacheable() {
    let server = base()
        .with_page_size(2)
        .with_cache(Duration::from_secs(300), CacheScope::Private);
    for name in ["delta", "alpha", "echo", "charlie", "bravo"] {
        server
            .tools()
            .add(Tool::new(name, Tier::ReadOnly), |_| Ok(ToolResult::new()))
            .unwrap();
    }
    let mut client = Client::new(&server);
    let mut names = Vec::new();
    let mut cursor: Option<String> = None;
    let mut id = 0;
    loop {
        id += 1;
        let params = match &cursor {
            Some(c) => format!(r#""cursor":"{c}""#),
            None => String::new(),
        };
        let reply = client.request(&modern(id, "tools/list", &params));
        let result = &reply["result"];
        assert_eq!(result["ttlMs"], 300_000);
        assert_eq!(
            result["cacheScope"], "private",
            "every page carries the same scope"
        );
        for tool in result["tools"].as_array().unwrap().iter() {
            names.push(tool["name"].as_str().unwrap().to_owned());
        }
        match result.get("nextCursor").and_then(|c| c.as_str()) {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(names, ["alpha", "bravo", "charlie", "delta", "echo"]);
    assert_eq!(id, 3);
}

#[test]
fn test_list_refuses_foreign_cursors() {
    let server = base();
    let mut client = Client::new(&server);
    for cursor in [r#""cursor":"garbage""#, r#""cursor":"""#, r#""cursor":5"#] {
        let reply = client.request(&modern(1, "tools/list", cursor));
        assert_eq!(code(&reply), Some(-32602), "{cursor}");
    }
    let reply = client.request(&modern(2, "tools/list", r#""cursor":null"#));
    assert!(reply["result"]["tools"].as_array().unwrap().is_empty());
}

#[test]
fn test_list_shows_schemas_and_annotations() {
    let server = base()
        .tool(
            adder()
                .title("Adder")
                .output_schema(json!({ "type": "object" })),
            add,
        )
        .unwrap()
        .tool(
            Tool::new("nuke", Tier::Destructive).idempotent(true),
            |_| Ok(ToolResult::new()),
        )
        .unwrap();
    let mut client = Client::new(&server);
    let reply = client.request(&modern(1, "tools/list", ""));
    let tools = reply["result"]["tools"].as_array().unwrap();
    assert_eq!(tools[0]["title"], "Adder");
    assert_eq!(
        tools[0]["inputSchema"]["required"].to_string(),
        r#"["a","b"]"#
    );
    assert_eq!(tools[0]["outputSchema"]["type"], "object");
    assert_eq!(
        tools[0]["annotations"].to_string(),
        r#"{"readOnlyHint":true}"#
    );
    assert_eq!(
        tools[1]["inputSchema"].to_string(),
        r#"{"type":"object","additionalProperties":false}"#
    );
    assert_eq!(
        tools[1]["annotations"].to_string(),
        r#"{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true}"#
    );
}

#[test]
fn test_call_success_and_tool_execution_errors() {
    let server = base().with_workers(0).tool(adder(), add).unwrap();
    let mut client = Client::new(&server);
    let reply = client.request(&call(1, "add", r#"{"a":2,"b":3}"#));
    assert_eq!(text(&reply), "5");
    assert!(reply["result"].get("isError").is_none());
    assert_eq!(reply["result"]["resultType"], "complete");
    // A handler error is a result the model can read, not a protocol error.
    let reply = client.request(&call(2, "add", r#"{"a":9223372036854775807,"b":1}"#));
    assert_eq!(reply["result"]["isError"], true);
    assert!(text(&reply).contains("overflows"));
}

#[test]
fn test_invalid_arguments_are_tool_errors_naming_the_problem() {
    let server = base().with_workers(0).tool(adder(), add).unwrap();
    let mut client = Client::new(&server);
    let reply = client.request(&call(1, "add", r#"{"a":"two","c":true}"#));
    assert_eq!(reply["result"]["isError"], true);
    let message = text(&reply);
    assert!(
        message.starts_with("Invalid arguments for the tool `add`: "),
        "{message}"
    );
    assert!(
        message.contains("/a: expected integer, found string"),
        "{message}"
    );
    assert!(message.contains("\"b\""), "{message}");
    assert!(
        message.contains("/c: is not an allowed property"),
        "{message}"
    );
    // Absent arguments are validated as `{}`.
    let reply = client.request(&modern(2, "tools/call", r#""name":"add""#));
    assert_eq!(reply["result"]["isError"], true);
}

#[test]
fn test_malformed_calls_and_unknown_tools_are_protocol_errors() {
    let server = base().with_workers(0).tool(adder(), add).unwrap();
    let mut client = Client::new(&server);
    let reply = client.request(&call(1, "nope", "{}"));
    assert_eq!(code(&reply), Some(-32602));
    assert_eq!(reply["error"]["message"], "Unknown tool: nope");
    let reply = client.request(&modern(2, "tools/call", r#""arguments":{}"#));
    assert_eq!(code(&reply), Some(-32602));
    let reply = client.request(&modern(3, "tools/call", r#""name":7"#));
    assert_eq!(code(&reply), Some(-32602));
    let reply = client.request(&call(4, "add", "[1,2]"));
    assert_eq!(code(&reply), Some(-32602));
    let reply = client.request(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":[1]}"#);
    assert_eq!(code(&reply), Some(-32602));
    let long = "x".repeat(5000);
    let reply = client.request(&call(6, &long, "{}"));
    assert!(
        reply["error"]["message"].as_str().unwrap().len() < 200,
        "peer text is clipped"
    );
}

#[test]
fn test_handler_protocol_errors_and_panics() {
    let server = base()
        .with_workers(0)
        .tool(Tool::new("protocol", Tier::ReadOnly), |_| {
            Err(ToolError::protocol(ErrorObject::internal_error(
                "index unavailable",
            )))
        })
        .unwrap()
        .tool(Tool::new("capability", Tier::ReadOnly), |call| {
            if call.client_capabilities().get("sampling").is_none() {
                return Err(ToolError::missing_capability(&json!({ "sampling": {} })));
            }
            Ok(ToolResult::new())
        })
        .unwrap()
        .tool(Tool::new("panics", Tier::ReadOnly), |_| {
            std::panic::panic_any("handler bug")
        })
        .unwrap();
    let mut client = Client::new(&server);
    let reply = client.request(&call(1, "protocol", "{}"));
    assert_eq!(
        (code(&reply), reply["error"]["message"].as_str()),
        (Some(-32603), Some("index unavailable"))
    );
    let reply = client.request(&call(2, "capability", "{}"));
    assert_eq!(code(&reply), Some(-32021));
    assert_eq!(
        reply["error"]["data"].to_string(),
        r#"{"requiredCapabilities":{"sampling":{}}}"#
    );
    let reply = client.request(&call(3, "panics", "{}"));
    assert_eq!(code(&reply), Some(-32603));
    assert!(
        !reply.to_string().contains("handler bug"),
        "panic text stays on the server"
    );
    // The connection carries on.
    let reply = client.request(&call(4, "protocol", "{}"));
    assert_eq!(code(&reply), Some(-32603));
}

#[test]
fn test_structured_content_and_output_schemas() {
    let schema = json!({ "type": "object", "properties": { "n": { "type": "integer" } }, "required": ["n"] });
    let server = base()
        .with_workers(0)
        .tool(
            Tool::new("good", Tier::ReadOnly).output_schema(schema.clone()),
            |_| Ok(ToolResult::structured(json!({ "n": 1 }))),
        )
        .unwrap()
        .tool(
            Tool::new("wrong", Tier::ReadOnly).output_schema(schema.clone()),
            |_| Ok(ToolResult::structured(json!({ "n": "one" }))),
        )
        .unwrap()
        .tool(
            Tool::new("missing", Tier::ReadOnly).output_schema(schema.clone()),
            |_| Ok(ToolResult::text("no data")),
        )
        .unwrap()
        .tool(
            Tool::new("failing", Tier::ReadOnly).output_schema(schema),
            |_| Ok(ToolResult::error("upstream down")),
        )
        .unwrap()
        .tool(Tool::new("rich", Tier::ReadOnly), |_| {
            Ok(ToolResult::text("see")
                .with(Content::image_bytes(b"\x89PNG", "image/png"))
                .with(Content::resource_link("file:///a.md", "a.md"))
                .with(Content::embedded_text("file:///b.md", "# B")))
        })
        .unwrap();
    let mut client = Client::new(&server);
    let reply = client.request(&call(1, "good", "{}"));
    assert_eq!(
        reply["result"]["structuredContent"].to_string(),
        r#"{"n":1}"#
    );
    assert_eq!(
        text(&reply),
        r#"{"n":1}"#,
        "a serialized text copy for older clients"
    );
    for (id, tool) in [(2, "wrong"), (3, "missing")] {
        let reply = client.request(&call(id, tool, "{}"));
        assert_eq!(code(&reply), Some(-32603), "{tool}");
    }
    let reply = client.request(&call(4, "failing", "{}"));
    assert_eq!(
        reply["result"]["isError"], true,
        "a tool error needs no structured content"
    );
    let reply = client.request(&call(5, "rich", "{}"));
    let content = reply["result"]["content"].as_array().unwrap();
    let kinds: Vec<&str> = content
        .iter()
        .map(|c| c["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["text", "image", "resource_link", "resource"]);
}

/// A tool that blocks until the test releases it, reporting when it starts.
fn blocking(
    started: mpsc::Sender<()>,
    release: Arc<Mutex<mpsc::Receiver<()>>>,
) -> impl Fn(&mcp_io::ToolCall<'_>) -> Result<ToolResult, ToolError> {
    move |call| {
        let _ = started.send(());
        let _ = release.lock().unwrap().recv();
        let cancelled = call.is_cancelled();
        let progress = call.progress(1.0, None, None);
        if cancelled {
            assert!(matches!(progress, Err(Error::Cancelled)));
        }
        Ok(ToolResult::text("finished"))
    }
}

#[test]
fn test_cancelled_call_gets_no_response() {
    let (started_tx, started) = mpsc::channel();
    let (release_tx, release) = mpsc::channel();
    let server = base()
        .tool(
            Tool::new("slow", Tier::ReadOnly),
            blocking(started_tx, Arc::new(Mutex::new(release))),
        )
        .unwrap()
        .tool(adder(), add)
        .unwrap();
    let mut client = Client::new(&server);
    client.send(&call(1, "slow", "{}"));
    started.recv_timeout(common::WAIT).unwrap();
    // The reading thread is free while the tool runs: a cancellation and
    // another call are both handled.
    client.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1,"reason":"user"}}"#);
    let reply = client.request(&call(2, "add", r#"{"a":1,"b":1}"#));
    assert_eq!(
        (reply["id"].as_i64(), text(&reply)),
        (Some(2), "2".to_owned())
    );
    release_tx.send(()).unwrap();
    assert!(client.connection.wait_idle(common::WAIT));
    assert!(
        client.try_recv(Duration::from_millis(200)).is_none(),
        "nothing is sent for a cancelled call"
    );
    // Unknown, finished, and malformed cancellations are ignored.
    client
        .send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":99}}"#);
    client.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}"#);
    client.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#);
    let reply = client.request(&call(3, "add", r#"{"a":2,"b":2}"#));
    assert_eq!(text(&reply), "4");
}

#[test]
fn test_progress_is_sent_only_when_asked_and_must_increase() {
    let server = base()
        .with_workers(0)
        .tool(Tool::new("work", Tier::ReadOnly), |call| {
            let wanted = call.wants_progress();
            call.progress(1.0, Some(3.0), Some("one"))?;
            call.progress(2.5, Some(3.0), None)?;
            let repeated = call.progress(2.5, None, None);
            assert!(matches!(repeated, Err(Error::ProgressNotIncreasing { .. })) || !wanted);
            let infinite = call.progress(f64::INFINITY, None, None);
            assert!(infinite.is_err() || !wanted);
            Ok(ToolResult::text(if wanted {
                "with progress"
            } else {
                "without"
            }))
        })
        .unwrap();
    let mut client = Client::new(&server);
    let reply = client.request(&call(1, "work", "{}"));
    assert_eq!(text(&reply), "without");
    assert_eq!(client.log.len(), 1, "no progress without a token");
    let request = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"progressToken":"tok"},"name":"work"}}"#;
    client.send(request);
    let first = client.recv();
    let second = client.recv();
    let reply = client.recv();
    assert_eq!(
        first.to_string(),
        r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"tok","progress":1,"total":3,"message":"one"}}"#
    );
    assert_eq!(second["params"]["progress"].as_f64(), Some(2.5));
    assert_eq!(
        text(&reply),
        "with progress",
        "the response comes after every progress report"
    );
    assert!(client.try_recv(Duration::from_millis(100)).is_none());
}

#[test]
fn test_subscriptions_acknowledge_then_tag_list_changes() {
    let server = base();
    let tools = server.tools();
    let mut client = Client::new(&server);
    client.send(&modern(
        7,
        "subscriptions/listen",
        r#""notifications":{"toolsListChanged":true,"promptsListChanged":true,"resourceSubscriptions":["file:///x"]}"#,
    ));
    let ack = client.recv();
    assert_eq!(ack["method"], "notifications/subscriptions/acknowledged");
    assert_eq!(
        ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        7
    );
    assert_eq!(
        ack["params"]["notifications"].to_string(),
        r#"{"toolsListChanged":true}"#,
        "only what the server supports is acknowledged"
    );
    tools
        .add(Tool::new("late", Tier::ReadOnly), |_| Ok(ToolResult::new()))
        .unwrap();
    let changed = client.recv();
    assert_eq!(
        changed.to_string(),
        r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{"_meta":{"io.modelcontextprotocol/subscriptionId":7}}}"#
    );
    assert!(tools.remove("late"));
    assert_eq!(client.recv()["method"], "notifications/tools/list_changed");
    // Cancelling the stream ends it: no response, no further notifications.
    client.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#);
    tools
        .add(
            Tool::new("later", Tier::ReadOnly),
            |_| Ok(ToolResult::new()),
        )
        .unwrap();
    assert!(client.try_recv(Duration::from_millis(200)).is_none());
}

#[test]
fn test_subscription_without_tools_changes_is_acknowledged_empty() {
    let server = base().with_list_changed(false);
    let tools = server.tools();
    let mut client = Client::new(&server);
    client.send(&modern(
        1,
        "subscriptions/listen",
        r#""notifications":{"toolsListChanged":true}"#,
    ));
    let ack = client.recv();
    assert_eq!(ack["params"]["notifications"].to_string(), "{}");
    tools
        .add(Tool::new("x", Tier::ReadOnly), |_| Ok(ToolResult::new()))
        .unwrap();
    assert!(
        client.try_recv(Duration::from_millis(100)).is_none(),
        "nothing the client was not promised"
    );
}

#[test]
fn test_subscription_refusals() {
    let server = base().with_max_subscriptions(1);
    let mut client = Client::new(&server);
    for params in [
        r#""#,
        r#""notifications":[]"#,
        r#""notifications":{"toolsListChanged":1}"#,
        r#""notifications":{"resourceSubscriptions":[1]}"#,
    ] {
        let reply = client.request(&modern(1, "subscriptions/listen", params));
        assert_eq!(code(&reply), Some(-32602), "{params}");
    }
    client.send(&modern(2, "subscriptions/listen", r#""notifications":{}"#));
    let _ack = client.recv();
    let reply = client.request(&modern(3, "subscriptions/listen", r#""notifications":{}"#));
    assert_eq!(code(&reply), Some(-32600), "over the per-connection limit");
}

#[test]
fn test_legacy_sessions_get_plain_list_changed_after_initialized() {
    let server = base();
    let tools = server.tools();
    let mut client = Client::new(&server);
    open_legacy(&mut client, "{}");
    tools
        .add(Tool::new("new", Tier::ReadOnly), |_| Ok(ToolResult::new()))
        .unwrap();
    assert_eq!(
        client.recv().to_string(),
        r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#
    );
    // A closed connection is forgotten.
    client.connection.close();
    tools
        .add(
            Tool::new("newer", Tier::ReadOnly),
            |_| Ok(ToolResult::new()),
        )
        .unwrap();
}

#[test]
fn test_busy_server_refuses_with_internal_error() {
    let (started_tx, started) = mpsc::channel();
    let (release_tx, release) = mpsc::channel();
    let server = base()
        .with_workers(1)
        .with_max_queued_calls(1)
        .tool(
            Tool::new("slow", Tier::ReadOnly),
            blocking(started_tx, Arc::new(Mutex::new(release))),
        )
        .unwrap();
    let mut client = Client::new(&server);
    client.send(&call(1, "slow", "{}"));
    started.recv_timeout(common::WAIT).unwrap();
    let reply = client.request(&call(2, "slow", "{}"));
    assert_eq!(code(&reply), Some(-32603));
    assert!(reply["error"]["message"].as_str().unwrap().contains("busy"));
    release_tx.send(()).unwrap();
    assert_eq!(text(&client.recv()), "finished");
}

#[test]
fn test_legacy_calls_use_the_session_capabilities() {
    let server = base()
        .with_workers(0)
        .tool(Tool::new("caps", Tier::ReadOnly), |call| {
            Ok(ToolResult::text(format!(
                "{} {}",
                call.protocol_version(),
                call.client_capabilities()
            )))
        })
        .unwrap();
    let mut client = Client::new(&server);
    open_legacy(&mut client, r#"{"roots":{}}"#);
    let reply = client
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"caps"}}"#);
    assert_eq!(text(&reply), r#"2025-11-25 {"roots":{}}"#);
    assert!(reply["result"].get("resultType").is_none());
    let reply = client.request(&call(2, "caps", "{}"));
    assert_eq!(text(&reply), "2026-07-28 {}");
}
