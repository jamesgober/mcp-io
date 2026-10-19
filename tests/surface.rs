//! The public surface: builders, accessors, `Debug`, and the I/O error paths
//! of `Connection::run`.

#![allow(clippy::unwrap_used)]

mod common;

use std::{
    io::{self, Read},
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{Client, call, code, modern, open_legacy, text};
use mcp_io::{
    Error, Implementation, RegisterError, SchemaErrorKind, SchemaLimits, Server, Tier, Tool,
    ToolResult, jsonx::json, protocol::Era,
};

#[test]
fn test_implementation_and_server_builders() {
    let info = Implementation::new("n", "1")
        .title("T")
        .description("D")
        .website_url("https://example.com");
    assert_eq!((info.name(), info.version()), ("n", "1"));
    assert!(
        info.to_json()
            .contains(r#""websiteUrl":"https://example.com""#)
    );
    let server = Server::new(info)
        .with_max_pending_confirmations(1)
        .with_schema_limits(SchemaLimits::new().with_max_depth(1));
    let deep = Tool::new("deep", Tier::ReadOnly).input_schema(json!({
        "type": "object",
        "properties": { "a": { "items": { "items": {} } } }
    }));
    let refused = server.tools().add(deep, |_| Ok(ToolResult::new()));
    assert!(matches!(
        refused,
        Err(RegisterError::Schema { error, .. }) if matches!(error.kind(), SchemaErrorKind::TooDeep { .. })
    ));
    server
        .tools()
        .add(Tool::new("ok", Tier::ReadOnly), |_| Ok(ToolResult::new()))
        .unwrap();
    let tools = server.tools();
    assert!(tools.contains("ok") && !tools.contains("deep"));
    assert_eq!(tools.len(), 1);
    assert_eq!(format!("{tools:?}"), r#"Tools { names: ["ok"] }"#);
    assert!(format!("{server:?}").contains(r#"tools: ["ok"]"#));
    assert!(!tools.remove("missing"));
}

#[test]
fn test_one_pending_confirmation_at_a_time() {
    let server = Server::new(Implementation::new("n", "1"))
        .with_max_pending_confirmations(1)
        .tool(Tool::new("ship", Tier::Publish), |_| {
            Ok(ToolResult::text("shipped"))
        })
        .unwrap();
    let mut client = Client::new(&server);
    let ask = |client: &mut Client, id| {
        let reply = client.request(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{"elicitation":{{}}}}}},"name":"ship"}}}}"#
        ));
        reply["result"]["requestState"].as_str().unwrap().to_owned()
    };
    let first = ask(&mut client, 1);
    let second = ask(&mut client, 2);
    let retry = |state: &str, id| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{"elicitation":{{}}}}}},"name":"ship","requestState":"{state}","inputResponses":{{"confirm":{{"action":"accept","content":{{"confirm":true}}}}}}}}}}"#
        )
    };
    assert_eq!(
        code(&client.request(&retry(&first, 3))),
        Some(-32602),
        "evicted by the second"
    );
    assert_eq!(text(&client.request(&retry(&second, 4))), "shipped");
}

#[test]
fn test_tool_call_accessors() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = Arc::clone(&seen);
    let server = Server::new(Implementation::new("n", "1"))
        .with_workers(0)
        .tool(Tool::new("look", Tier::Mutating), move |call| {
            let token = call.cancel_token();
            log.lock().unwrap().push(format!(
                "{} {} {} {} {:?} {} {}",
                call.name(),
                call.tier(),
                call.id(),
                call.era() == Era::Modern,
                call.client_info().map(|i| i["name"].to_string()),
                token.is_cancelled(),
                format!("{call:?}").starts_with("ToolCall {"),
            ));
            Ok(ToolResult::new())
        })
        .unwrap();
    let mut client = Client::new(&server);
    let _ = client.request(&call(7, "look", "{}"));
    open_legacy(&mut client, "{}");
    let _ = client
        .request(r#"{"jsonrpc":"2.0","id":"s","method":"tools/call","params":{"name":"look"}}"#);
    assert_eq!(
        *seen.lock().unwrap(),
        [
            r#"look mutating 7 true Some("\"test-client\"") false true"#,
            r#"look mutating "s" false Some("\"legacy-client\"") false true"#,
        ]
    );
}

#[test]
fn test_long_arguments_are_shortened_in_the_confirmation() {
    let server = Server::new(Implementation::new("n", "1"))
        .with_workers(0)
        .tool(
            Tool::new("ship", Tier::Publish).input_schema(json!({ "type": "object" })),
            |_| Ok(ToolResult::new()),
        )
        .unwrap();
    let mut client = Client::new(&server);
    let long = "x".repeat(5000);
    let reply = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{"elicitation":{{}}}}}},"name":"ship","arguments":{{"note":"{long}"}}}}}}"#
    ));
    let message = reply["result"]["inputRequests"]["confirm"]["params"]["message"]
        .as_str()
        .unwrap();
    assert!(message.contains("Arguments (shortened): "));
    assert!(message.len() < 2300);
}

#[test]
fn test_legacy_elicitation_answered_with_an_error() {
    let server = Server::new(Implementation::new("n", "1"))
        .tool(Tool::new("ship", Tier::Publish), |_| {
            Ok(ToolResult::text("shipped"))
        })
        .unwrap();
    let mut client = Client::new(&server);
    open_legacy(&mut client, r#"{"elicitation":{}}"#);
    client.send(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"ship"}}"#);
    let request = client.recv();
    let id = request["id"].to_string();
    client.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32601,"message":"no"}}}}"#
    ));
    let reply = client.recv();
    assert_eq!(reply["result"]["isError"], true);
    assert!(text(&reply).contains("not answered"));
    // An answer that is not an ElicitResult declines too.
    client.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ship"}}"#);
    let request = client.recv();
    let id = request["id"].to_string();
    client.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"action":"accept"}}}}"#
    ));
    assert_eq!(client.recv()["result"]["isError"], true);
}

#[test]
fn test_connection_accessors_and_close() {
    let server = Server::new(Implementation::new("n", "1"));
    let mut client = Client::new(&server);
    let _ = client.request(&modern(1, "tools/list", ""));
    assert!(!client.connection.is_closed());
    assert!(!client.connection.peer().is_closed());
    assert_eq!(client.connection.stats().requests_in, 1);
    client.connection.close();
    assert!(client.connection.is_closed());
    assert!(matches!(
        client.connection.handle(b"{}"),
        Err(Error::Rpc(_))
    ));
    client.connection.close();
}

/// A reader that is interrupted once, then fails.
struct Flaky(u8);

impl Read for Flaky {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0 += 1;
        match self.0 {
            1 => Err(io::ErrorKind::Interrupted.into()),
            2 => {
                let line = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
                buf[..line.len()].copy_from_slice(line);
                Ok(line.len())
            }
            _ => Err(io::Error::other("the pipe broke")),
        }
    }
}

#[test]
fn test_run_retries_interrupted_reads_and_reports_failures() {
    let server = Server::new(Implementation::new("n", "1"));
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut connection = server.connect(tx).unwrap();
    let end = connection.run(Flaky(0));
    assert!(
        matches!(&end, Err(Error::Rpc(rpc_io::Error::Io(e))) if e.to_string() == "the pipe broke"),
        "{end:?}"
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).ok().as_deref(),
        Some(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
    );
    assert!(connection.is_closed());
    let display = Error::Rpc(rpc_io::Error::Closed);
    assert!(std::error::Error::source(&display).is_some());
}

#[test]
fn test_an_unbounded_confirmation_timeout_is_accepted() {
    let server = Server::new(Implementation::new("n", "1"))
        .with_confirmation_timeout(Duration::MAX)
        .with_workers(0)
        .tool(Tool::new("ship", Tier::Publish), |_| {
            Ok(ToolResult::text("shipped"))
        })
        .unwrap();
    let mut client = Client::new(&server);
    let reply = client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{"elicitation":{}}},"name":"ship"}}"#,
    );
    assert_eq!(reply["result"]["resultType"], "input_required");
}

#[test]
fn test_queued_bytes_are_bounded() {
    let (started_tx, started) = std::sync::mpsc::channel::<()>();
    let (release_tx, release) = std::sync::mpsc::channel::<()>();
    let release = Mutex::new(release);
    let server = Server::new(Implementation::new("n", "1"))
        .with_workers(1)
        .with_max_queued_bytes(1000)
        .tool(
            Tool::new("slow", Tier::ReadOnly).input_schema(json!({ "type": "object" })),
            move |_| {
                let _ = started_tx.send(());
                let _ = release.lock().unwrap().recv_timeout(common::WAIT);
                Ok(ToolResult::text("done"))
            },
        )
        .unwrap();
    let mut client = Client::new(&server);
    let pad = "x".repeat(2000);
    client.send(&call(1, "slow", &format!(r#"{{"pad":"{pad}"}}"#)));
    started.recv_timeout(common::WAIT).unwrap();
    let reply = client.request(&call(2, "slow", r#"{"pad":"small"}"#));
    assert_eq!(
        code(&reply),
        Some(-32603),
        "the first call holds more than the limit"
    );
    release_tx.send(()).unwrap();
    assert_eq!(text(&client.recv()), "done");
}
