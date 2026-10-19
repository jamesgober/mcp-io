//! Per-request versioning (2026-07-28) and the legacy `initialize` lifecycle
//! (2025-11-25 and 2025-06-18), including every refusal.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Client, INITIALIZED, code, initialize, meta, modern, open_legacy};
use mcp_io::{Implementation, Lifecycle, Server, Tier, Tool, ToolResult};

fn server() -> Server {
    Server::new(Implementation::new("lifecycle-test", "2.0.0"))
        .instructions("Use `echo`.")
        .with_workers(0)
        .tool(Tool::new("echo", Tier::ReadOnly), |_| {
            Ok(ToolResult::text("echo"))
        })
        .unwrap()
}

#[test]
fn test_modern_requests_need_no_handshake_and_leave_no_state() {
    let server = server();
    let mut client = Client::new(&server);
    let reply = client.request(&modern(1, "tools/list", ""));
    assert_eq!(reply["result"]["tools"][0]["name"], "echo");
    assert_eq!(reply["result"]["resultType"], "complete");
    assert_eq!(
        reply["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "lifecycle-test"
    );
    assert_eq!(client.connection.lifecycle(), &Lifecycle::Stateless);
}

#[test]
fn test_discover_advertises_versions_capabilities_and_cache_hints() {
    let server = server();
    let mut client = Client::new(&server);
    let reply = client.request(&modern(1, "server/discover", ""));
    let result = &reply["result"];
    assert_eq!(result["supportedVersions"].to_string(), r#"["2026-07-28"]"#);
    assert_eq!(
        result["capabilities"].to_string(),
        r#"{"tools":{"listChanged":true}}"#
    );
    assert_eq!(result["instructions"], "Use `echo`.");
    assert_eq!(result["ttlMs"], 0);
    assert_eq!(result["cacheScope"], "public");
    assert_eq!(result["resultType"], "complete");
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["version"],
        "2.0.0"
    );
}

#[test]
fn test_missing_meta_fields_are_invalid_params() {
    let server = server();
    let mut client = Client::new(&server);
    // No _meta at all, no session: -32602, naming the missing field.
    let reply = client.request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    assert_eq!(code(&reply), Some(-32602));
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("protocolVersion")
    );
    // A version but no capabilities.
    let reply = client.request(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#,
    );
    assert_eq!(code(&reply), Some(-32602));
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("clientCapabilities")
    );
    // A recognised version with an unrecognised log level.
    let reply = client.request(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/logLevel":"loud"}}}"#,
    );
    assert_eq!(code(&reply), Some(-32602));
}

#[test]
fn test_unsupported_version_lists_the_supported_ones() {
    let server = server();
    let mut client = Client::new(&server);
    let reply = client.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1900-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}"#,
    );
    assert_eq!(code(&reply), Some(-32022));
    assert_eq!(reply["error"]["message"], "Unsupported protocol version");
    assert_eq!(
        reply["error"]["data"].to_string(),
        r#"{"supported":["2026-07-28"],"requested":"1900-01-01"}"#
    );
}

#[test]
fn test_legacy_lifecycle_moves_through_its_states() {
    let server = server();
    let mut client = Client::new(&server);
    assert_eq!(client.connection.lifecycle(), &Lifecycle::Stateless);
    // `initialized` before `initialize` changes nothing.
    client.send(INITIALIZED);
    assert_eq!(client.connection.lifecycle(), &Lifecycle::Stateless);
    let reply = client.request(&initialize(1, "2025-11-25", r#"{"elicitation":{}}"#));
    let result = &reply["result"];
    assert_eq!(result["protocolVersion"], "2025-11-25");
    assert_eq!(
        result["capabilities"].to_string(),
        r#"{"tools":{"listChanged":true}}"#
    );
    assert_eq!(result["serverInfo"]["name"], "lifecycle-test");
    assert_eq!(result["instructions"], "Use `echo`.");
    assert!(
        result.get("resultType").is_none(),
        "legacy results carry no resultType"
    );
    assert!(result.get("_meta").is_none());
    assert_eq!(
        client.connection.lifecycle(),
        &Lifecycle::Initializing {
            protocol_version: "2025-11-25"
        }
    );
    // Requests are served as soon as the client has the response.
    let reply = client.request(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    assert_eq!(reply["result"]["tools"][0]["name"], "echo");
    assert!(
        reply["result"].get("ttlMs").is_none(),
        "no cache hints before 2026-07-28"
    );
    client.send(INITIALIZED);
    assert_eq!(
        client.connection.lifecycle(),
        &Lifecycle::Operating {
            protocol_version: "2025-11-25"
        }
    );
    client.send(INITIALIZED);
    assert_eq!(
        client.connection.lifecycle().protocol_version(),
        Some("2025-11-25")
    );
}

#[test]
fn test_initialize_negotiates_the_version() {
    for (requested, negotiated) in [
        ("2025-11-25", "2025-11-25"),
        ("2025-06-18", "2025-06-18"),
        // Older (batch-era) and unknown versions are offered the newest
        // legacy revision; the client decides.
        ("2025-03-26", "2025-11-25"),
        ("2024-11-05", "2025-11-25"),
        ("2099-01-01", "2025-11-25"),
    ] {
        let server = server();
        let mut client = Client::new(&server);
        let reply = client.request(&initialize(1, requested, "{}"));
        assert_eq!(
            reply["result"]["protocolVersion"], negotiated,
            "{requested}"
        );
    }
}

#[test]
fn test_initialize_refusals() {
    let server = server();
    let mut client = Client::new(&server);
    for params in [
        r#"{"capabilities":{},"clientInfo":{"name":"c","version":"1"}}"#,
        r#"{"protocolVersion":7,"capabilities":{},"clientInfo":{"name":"c","version":"1"}}"#,
        r#"{"protocolVersion":"2025-11-25","clientInfo":{"name":"c","version":"1"}}"#,
        r#"{"protocolVersion":"2025-11-25","capabilities":[],"clientInfo":{"name":"c","version":"1"}}"#,
        r#"{"protocolVersion":"2025-11-25","capabilities":{}}"#,
    ] {
        let reply = client.request(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{params}}}"#
        ));
        assert_eq!(code(&reply), Some(-32602), "{params}");
        assert_eq!(client.connection.lifecycle(), &Lifecycle::Stateless);
    }
    open_legacy(&mut client, "{}");
    let reply = client.request(&initialize(9, "2025-11-25", "{}"));
    assert_eq!(code(&reply), Some(-32600), "a second initialize");
}

#[test]
fn test_modern_only_server_answers_initialize_with_the_supported_versions() {
    let server = server().with_legacy(false);
    let mut client = Client::new(&server);
    let reply = client.request(&initialize(1, "2025-11-25", "{}"));
    assert_eq!(code(&reply), Some(-32022));
    assert_eq!(
        reply["error"]["data"].to_string(),
        r#"{"supported":["2026-07-28"],"requested":"2025-11-25"}"#
    );
    assert_eq!(client.connection.lifecycle(), &Lifecycle::Stateless);
    // And `ping`, a legacy method, does not exist for it.
    let reply = client.request(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#);
    assert_eq!(code(&reply), Some(-32601));
    let reply = client.request(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
    assert_eq!(code(&reply), Some(-32602));
    assert!(
        !reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("initialize")
    );
}

#[test]
fn test_ping_is_legacy_only() {
    let server = server();
    let mut client = Client::new(&server);
    // Allowed before initialize.
    let reply = client.request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    assert_eq!(reply["result"].to_string(), "{}");
    // 2026-07-28 removed it.
    let reply = client.request(&modern(2, "ping", ""));
    assert_eq!(code(&reply), Some(-32601));
}

#[test]
fn test_modern_only_methods_are_unknown_to_a_legacy_session() {
    let server = server();
    let mut client = Client::new(&server);
    open_legacy(&mut client, "{}");
    for method in ["server/discover", "subscriptions/listen"] {
        let reply = client.request(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"{method}","params":{{"notifications":{{}}}}}}"#
        ));
        assert_eq!(code(&reply), Some(-32601), "{method}");
    }
}

#[test]
fn test_both_eras_share_one_connection() {
    let server = server();
    let mut client = Client::new(&server);
    open_legacy(&mut client, "{}");
    let legacy = client.request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let modern_reply = client.request(&modern(2, "tools/list", ""));
    assert!(legacy["result"].get("resultType").is_none());
    assert_eq!(modern_reply["result"]["resultType"], "complete");
}

#[test]
fn test_unknown_methods_batches_and_bad_json() {
    let server = server();
    let mut client = Client::new(&server);
    let reply = client.request(&modern(1, "resources/list", ""));
    assert_eq!(code(&reply), Some(-32601));
    // No JSON-RPC batches since 2025-06-18; an unreadable message's error
    // carries no id (never `null`).
    let reply = client.request(&format!("[{}]", modern(2, "tools/list", "")));
    assert_eq!(code(&reply), Some(-32600));
    assert!(reply.get("id").is_none());
    let reply = client.request("{not json");
    assert_eq!(code(&reply), Some(-32700));
    assert!(reply.get("id").is_none());
    // A null id is not an MCP request id.
    let reply = client.request(&format!(
        r#"{{"jsonrpc":"2.0","id":null,"method":"tools/list","params":{{{}}}}}"#,
        meta()
    ));
    assert_eq!(code(&reply), Some(-32600));
}

#[test]
fn test_server_info_meta_can_be_turned_off() {
    let server = server()
        .with_server_info_meta(false)
        .with_list_changed(false);
    let mut client = Client::new(&server);
    let reply = client.request(&modern(1, "server/discover", ""));
    assert!(reply["result"].get("_meta").is_none());
    assert_eq!(
        reply["result"]["capabilities"].to_string(),
        r#"{"tools":{}}"#
    );
}
