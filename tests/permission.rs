//! The permission gate: policy decisions, confirmation through MRTR
//! elicitation (2026-07-28) and server-sent elicitation (legacy), the
//! single-use `requestState`, and the audit trail.

#![allow(clippy::unwrap_used)]

mod common;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use common::{Client, call, code, meta_with, open_legacy, text};
use mcp_io::{
    Ceiling, Decision, Implementation, Outcome, PermissionRequest, Server, Tier, Tool, ToolResult,
    jsonx::{Value, json},
};

/// A server with one tool per tier; `runs` counts handler executions.
fn server(runs: &Arc<AtomicUsize>) -> Server {
    let mut server = Server::new(Implementation::new("gate-test", "1.0.0"));
    for (name, tier) in [
        ("read", Tier::ReadOnly),
        ("edit", Tier::Mutating),
        ("delete", Tier::Destructive),
        ("publish", Tier::Publish),
    ] {
        let runs = Arc::clone(runs);
        server = server
            .tool(
                Tool::new(name, tier)
                    .confirmation(format!("Really {name}?"))
                    .input_schema(json!({ "type": "object", "properties": { "target": { "type": "string" } } })),
                move |call| {
                    let _ = runs.fetch_add(1, Ordering::SeqCst);
                    Ok(ToolResult::text(format!("{} ran", call.name())))
                },
            )
            .unwrap();
    }
    server
}

const FORM: &str = r#"{"elicitation":{"form":{}}}"#;

/// A 2026-07-28 `tools/call` with these capabilities and extra params.
fn call_with(id: u64, tool: &str, capabilities: &str, extra: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{{},"name":"{tool}","arguments":{{"target":"prod"}}{extra}}}}}"#,
        meta_with(capabilities)
    )
}

fn accept(state: &str) -> String {
    format!(
        r#","requestState":"{state}","inputResponses":{{"confirm":{{"action":"accept","content":{{"confirm":true}}}}}}"#
    )
}

#[test]
fn test_default_policy_runs_low_tiers_and_asks_for_high_ones() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs);
    let mut client = Client::new(&server);
    for (id, tool) in [(1, "read"), (2, "edit")] {
        let reply = client.request(&call_with(id, tool, FORM, ""));
        assert_eq!(text(&reply), format!("{tool} ran"));
    }
    for (id, tool) in [(3, "delete"), (4, "publish")] {
        let reply = client.request(&call_with(id, tool, FORM, ""));
        assert_eq!(reply["result"]["resultType"], "input_required", "{tool}");
    }
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[test]
fn test_confirmation_round_trip() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs);
    let mut client = Client::new(&server);
    let reply = client.request(&call_with(1, "publish", FORM, ""));
    let result = &reply["result"];
    assert_eq!(result["resultType"], "input_required");
    let request = &result["inputRequests"]["confirm"];
    assert_eq!(request["method"], "elicitation/create");
    assert_eq!(request["params"]["mode"], "form");
    let message = request["params"]["message"].as_str().unwrap();
    assert!(message.starts_with("Really publish?"), "{message}");
    assert!(
        message.contains("Tool: publish (publish tier)"),
        "{message}"
    );
    assert!(
        message.contains(r#"{"target":"prod"}"#),
        "the arguments are shown: {message}"
    );
    assert_eq!(
        request["params"]["requestedSchema"]["required"].to_string(),
        r#"["confirm"]"#
    );
    let state = result["requestState"].as_str().unwrap().to_owned();
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    // The retry (a new id) with the user's acceptance runs the tool once.
    let reply = client.request(&call_with(2, "publish", FORM, &accept(&state)));
    assert_eq!(text(&reply), "publish ran");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    // The same state again is a replay: refused, not run.
    let reply = client.request(&call_with(3, "publish", FORM, &accept(&state)));
    assert_eq!(code(&reply), Some(-32602));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[test]
fn test_confirmation_is_bound_to_the_tool_and_arguments() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs);
    let mut client = Client::new(&server);
    let state = |client: &mut Client, id| {
        let reply = client.request(&call_with(id, "publish", FORM, ""));
        reply["result"]["requestState"].as_str().unwrap().to_owned()
    };
    // Other arguments.
    let s = state(&mut client, 1);
    let tampered = call_with(2, "publish", FORM, &accept(&s))
        .replace(r#""target":"prod""#, r#""target":"everything""#);
    assert_eq!(code(&client.request(&tampered)), Some(-32602));
    // Another tool.
    let s = state(&mut client, 3);
    assert_eq!(
        code(&client.request(&call_with(4, "delete", FORM, &accept(&s)))),
        Some(-32602)
    );
    // A forged or altered state.
    let s = state(&mut client, 5);
    let altered = format!("{s}0");
    assert_eq!(
        code(&client.request(&call_with(6, "publish", FORM, &accept(&altered)))),
        Some(-32602)
    );
    assert_eq!(
        code(&client.request(&call_with(
            7,
            "publish",
            FORM,
            &accept("mcp-io.confirm.0000")
        ))),
        Some(-32602)
    );
    let wrong_type = r#","requestState":7"#;
    assert_eq!(
        code(&client.request(&call_with(8, "publish", FORM, wrong_type))),
        Some(-32602)
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[test]
fn test_declined_and_missing_answers() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs);
    let mut client = Client::new(&server);
    let state = |client: &mut Client, id| {
        let reply = client.request(&call_with(id, "delete", FORM, ""));
        reply["result"]["requestState"].as_str().unwrap().to_owned()
    };
    for (id, response) in [
        (2, r#"{"action":"decline"}"#),
        (4, r#"{"action":"cancel"}"#),
        (6, r#"{"action":"accept","content":{"confirm":false}}"#),
    ] {
        let s = state(&mut client, id - 1);
        let extra = format!(r#","requestState":"{s}","inputResponses":{{"confirm":{response}}}"#);
        let reply = client.request(&call_with(id, "delete", FORM, &extra));
        assert_eq!(reply["result"]["isError"], true, "{response}");
        assert!(text(&reply).contains("declined"));
    }
    // A retry without an answer is asked again, with a fresh state.
    let s = state(&mut client, 7);
    let reply = client.request(&call_with(
        8,
        "delete",
        FORM,
        &format!(r#","requestState":"{s}""#),
    ));
    assert_eq!(reply["result"]["resultType"], "input_required");
    assert_ne!(reply["result"]["requestState"].as_str(), Some(s.as_str()));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[test]
fn test_confirmation_needs_form_elicitation() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs);
    let mut client = Client::new(&server);
    for capabilities in ["{}", r#"{"elicitation":{"url":{}}}"#, r#"{"sampling":{}}"#] {
        let reply = client.request(&call_with(1, "publish", capabilities, ""));
        assert_eq!(code(&reply), Some(-32021), "{capabilities}");
        assert_eq!(
            reply["error"]["data"].to_string(),
            r#"{"requiredCapabilities":{"elicitation":{"form":{}}}}"#
        );
    }
    // `elicitation: {}` means form mode.
    let reply = client.request(&call_with(2, "publish", r#"{"elicitation":{}}"#, ""));
    assert_eq!(reply["result"]["resultType"], "input_required");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[test]
fn test_publish_always_asks_even_when_the_policy_allows() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs).policy(|_: &PermissionRequest<'_>| Decision::Allow);
    let mut client = Client::new(&server);
    let reply = client.request(&call_with(1, "delete", FORM, ""));
    assert_eq!(
        text(&reply),
        "delete ran",
        "the policy may waive confirmation below publish"
    );
    let reply = client.request(&call_with(2, "publish", FORM, ""));
    assert_eq!(reply["result"]["resultType"], "input_required");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[test]
fn test_the_policy_decides_a_confirmed_call_once() {
    // A budget of two confirmed calls allows two, not one: the retry that
    // carries the confirmation is not charged again.
    let runs = Arc::new(AtomicUsize::new(0));
    let decided = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&decided);
    let left = Mutex::new(2u32);
    let server = server(&runs).policy(move |_: &PermissionRequest<'_>| {
        let _ = seen.fetch_add(1, Ordering::SeqCst);
        let mut left = left.lock().unwrap();
        match left.checked_sub(1) {
            Some(rest) => {
                *left = rest;
                Decision::Confirm
            }
            None => Decision::deny("the budget is spent"),
        }
    });
    let mut client = Client::new(&server);
    for round in 0..2u64 {
        let reply = client.request(&call_with(round * 2 + 1, "edit", FORM, ""));
        let state = reply["result"]["requestState"].as_str().unwrap().to_owned();
        let reply = client.request(&call_with(round * 2 + 2, "edit", FORM, &accept(&state)));
        assert_eq!(text(&reply), "edit ran", "round {round}");
    }
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert_eq!(decided.load(Ordering::SeqCst), 2, "once per confirmed call");
    let reply = client.request(&call_with(5, "edit", FORM, ""));
    assert_eq!(text(&reply), "Permission denied: the budget is spent");
    // A retry is not a way around the policy: a state it never issued is
    // refused, even for a tool the policy would allow outright.
    let reply = client.request(&call_with(6, "read", FORM, &accept("forged")));
    assert_eq!(code(&reply), Some(-32602));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[test]
fn test_policy_denials_and_panics_are_tool_errors() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs).policy(|request: &PermissionRequest<'_>| {
        assert_eq!(
            request.client_info().and_then(|c| c["name"].as_str()),
            Some("test-client")
        );
        match request.tool() {
            "read" => Decision::deny("reads are paused"),
            "edit" => std::panic::panic_any("policy bug"),
            _ => Decision::Allow,
        }
    });
    let mut client = Client::new(&server);
    let reply = client.request(&call_with(1, "read", FORM, ""));
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(text(&reply), "Permission denied: reads are paused");
    let reply = client.request(&call_with(2, "edit", FORM, ""));
    assert_eq!(
        text(&reply),
        "Permission denied: the server's permission policy failed"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[test]
fn test_ceiling_policy() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs).policy(Ceiling(Tier::ReadOnly));
    let mut client = Client::new(&server);
    assert_eq!(
        text(&client.request(&call_with(1, "read", FORM, ""))),
        "read ran"
    );
    assert_eq!(
        client.request(&call_with(2, "edit", FORM, ""))["result"]["resultType"],
        "input_required"
    );
}

#[test]
fn test_validation_runs_before_the_gate() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs);
    let mut client = Client::new(&server);
    let bad = call_with(1, "publish", FORM, "").replace(r#"{"target":"prod"}"#, r#"{"target":7}"#);
    let reply = client.request(&bad);
    assert_eq!(
        reply["result"]["isError"], true,
        "invalid input never reaches a confirmation prompt"
    );
}

#[test]
fn test_legacy_confirmation_asks_with_a_server_request() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs);
    let mut client = Client::new(&server);
    open_legacy(&mut client, r#"{"elicitation":{}}"#);
    client.send(r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"publish","arguments":{"target":"prod"}}}"#);
    let request = client.recv();
    assert_eq!(request["method"], "elicitation/create");
    assert!(
        request["params"].get("mode").is_none(),
        "legacy form requests omit `mode`"
    );
    assert!(
        request["params"]["message"]
            .as_str()
            .unwrap()
            .contains("Really publish?")
    );
    let id = request["id"].to_string();
    client.send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"action":"accept","content":{{"confirm":true}}}}}}"#));
    let reply = client.recv();
    assert_eq!(
        (reply["id"].as_i64(), text(&reply)),
        (Some(10), "publish ran".to_owned())
    );
    // Declined.
    client.send(r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"delete","arguments":{}}}"#);
    let request = client.recv();
    let id = request["id"].to_string();
    client.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"action":"decline"}}}}"#
    ));
    let reply = client.recv();
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[test]
fn test_legacy_confirmation_refusals() {
    let runs = Arc::new(AtomicUsize::new(0));
    // No elicitation capability.
    let s = server(&runs);
    let mut client = Client::new(&s);
    open_legacy(&mut client, "{}");
    let reply = client
        .request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"publish"}}"#);
    assert_eq!(reply["result"]["isError"], true);
    assert!(text(&reply).contains("elicitation"));
    assert!(
        !text(&reply).contains("  "),
        "no run of spaces: {}",
        text(&reply)
    );
    // No workers: the answer cannot be waited for.
    let s = server(&runs).with_workers(0);
    let mut client = Client::new(&s);
    open_legacy(&mut client, r#"{"elicitation":{}}"#);
    let reply = client
        .request(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"publish"}}"#);
    assert_eq!(reply["result"]["isError"], true);
    assert!(text(&reply).contains("worker threads"));
    assert!(
        !text(&reply).contains("  "),
        "no run of spaces: {}",
        text(&reply)
    );
    assert_eq!(
        client
            .log
            .iter()
            .filter(|m| m.contains("elicitation/create"))
            .count(),
        0
    );
    // Timed out.
    let s = server(&runs).with_confirmation_timeout(Duration::from_millis(50));
    let mut client = Client::new(&s);
    open_legacy(&mut client, r#"{"elicitation":{}}"#);
    client.send(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"publish"}}"#);
    assert_eq!(client.recv()["method"], "elicitation/create");
    let mut reply = client.recv();
    if reply["method"] == "notifications/cancelled" {
        reply = client.recv();
    }
    assert!(text(&reply).contains("in time"), "{reply}");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[test]
fn test_confirmations_expire() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs).with_confirmation_timeout(Duration::ZERO);
    let mut client = Client::new(&server);
    let reply = client.request(&call_with(1, "publish", FORM, ""));
    let state = reply["result"]["requestState"].as_str().unwrap().to_owned();
    assert_eq!(
        code(&client.request(&call_with(2, "publish", FORM, &accept(&state)))),
        Some(-32602)
    );
}

#[test]
fn test_audit_records_every_outcome_without_arguments() {
    let runs = Arc::new(AtomicUsize::new(0));
    let records: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&records);
    let server = server(&runs).with_workers(0).on_audit(move |record| {
        seen.lock().unwrap().push(format!(
            "{} {} {} {:?} {}",
            record.tool().unwrap_or("-"),
            record.tier().map_or("-", |tier| tier.name()),
            record.id(),
            record.client(),
            record.outcome()
        ));
        assert!(record.elapsed() < Duration::from_secs(60));
    });
    let mut client = Client::new(&server);
    let _ = client.request(&call_with(1, "read", FORM, ""));
    let _ = client.request(&call(2, "read", r#"{"target":1}"#));
    let reply = client.request(&call_with(3, "publish", FORM, ""));
    let state = reply["result"]["requestState"].as_str().unwrap().to_owned();
    let _ = client.request(&call_with(4, "publish", FORM, &accept(&state)));
    let _ = client.request(&call_with(5, "publish", "{}", ""));
    // Refused requests are audited too: an unknown tool, no name, no _meta.
    let _ = client.request(&call(6, "nope", "{}"));
    let _ = client.request(&common::modern(7, "tools/call", r#""arguments":{}"#));
    let _ = client
        .request(r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"read"}}"#);
    let records = records.lock().unwrap().clone();
    assert_eq!(
        records,
        [
            "read read-only 1 Some(\"test-client\") completed",
            "read read-only 2 Some(\"test-client\") refused: invalid arguments",
            "publish publish 3 Some(\"test-client\") waiting for the user's confirmation",
            "publish publish 4 Some(\"test-client\") completed",
            "publish publish 5 Some(\"test-client\") not confirmed",
            "- - 6 None refused: invalid request",
            "- - 7 None refused: invalid request",
            "- - 8 None refused: invalid request",
        ]
    );
    assert!(
        records.iter().all(|r| !r.contains("prod")),
        "arguments never reach the audit trail"
    );
    let _ = Outcome::Cancelled;
    let _: Option<Value<'static>> = None;
}

#[test]
fn test_a_panicking_audit_hook_is_contained() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs)
        .with_workers(0)
        .on_audit(|_| std::panic::panic_any("hook bug"));
    let mut client = Client::new(&server);
    assert_eq!(
        text(&client.request(&call_with(1, "read", FORM, ""))),
        "read ran"
    );
    assert_eq!(
        text(&client.request(&call_with(2, "read", FORM, ""))),
        "read ran"
    );
}

#[test]
fn test_cancelling_a_call_stops_its_legacy_confirmation_wait() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs).with_workers(1);
    let mut client = Client::new(&server);
    open_legacy(&mut client, r#"{"elicitation":{}}"#);
    client.send(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"publish"}}"#);
    let request = client.recv();
    assert_eq!(request["method"], "elicitation/create");
    // The client cancels the call instead of answering: the worker stops
    // waiting, withdraws the elicitation, and sends nothing for the call.
    client.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#);
    let withdrawn = client.recv();
    assert_eq!(withdrawn["method"], "notifications/cancelled");
    assert_eq!(withdrawn["params"]["requestId"], request["id"].clone());
    assert!(
        client.connection.wait_idle(common::WAIT),
        "the worker is free again"
    );
    let reply = client
        .request(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read"}}"#);
    assert_eq!(text(&reply), "read ran", "and serves the next call");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[test]
fn test_a_confirmation_does_not_carry_over_to_a_re_registered_tool() {
    let runs = Arc::new(AtomicUsize::new(0));
    let server = server(&runs);
    let mut client = Client::new(&server);
    let reply = client.request(&call_with(1, "publish", FORM, ""));
    let state = reply["result"]["requestState"].as_str().unwrap().to_owned();
    let tools = server.tools();
    assert!(tools.remove("publish"));
    tools
        .add(
            Tool::new("publish", Tier::Publish).input_schema(json!({ "type": "object" })),
            |_| Ok(ToolResult::text("a different publish")),
        )
        .unwrap();
    let reply = client.request(&call_with(2, "publish", FORM, &accept(&state)));
    assert_eq!(code(&reply), Some(-32602));
}
