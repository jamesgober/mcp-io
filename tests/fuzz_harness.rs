//! The fuzz targets' checks (`tests/harness/mod.rs`), run in plain
//! `cargo test` on random bytes, on MCP-shaped messages with random parts,
//! and on mutations of valid messages. libFuzzer does not run on Windows
//! MSVC; this keeps the checks exercised on every platform.

#![allow(clippy::unwrap_used)]

mod harness;

use std::sync::OnceLock;

use mcp_io::Server;
use proptest::prelude::*;

fn server() -> &'static Server {
    static SERVER: OnceLock<Server> = OnceLock::new();
    SERVER.get_or_init(|| {
        // The harness's `panics` tool panics on purpose; keep that message
        // out of the test output, and report every other panic.
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if info.payload().downcast_ref::<&str>() != Some(&"fuzz") {
                default(info);
            }
        }));
        harness::server()
    })
}

const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{"elicitation":{}},"progressToken":1}"#;

/// Valid messages of every kind the server handles.
fn templates() -> Vec<String> {
    vec![
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{{{META}}}}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{{META},"cursor":"after:edit"}}}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{{META},"name":"read","arguments":{{"id":3,"tags":["a","b"],"mode":1.5,"when":0.75}}}}}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{{META},"name":"publish","arguments":{{}}}}}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{{META},"name":"publish","requestState":"mcp-io.confirm.x","inputResponses":{{"confirm":{{"action":"accept","content":{{"confirm":true}}}}}}}}}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":6,"method":"subscriptions/listen","params":{{{META},"notifications":{{"toolsListChanged":true}}}}}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{{META},"name":"shaped","arguments":{{"bad":1}}}}}}"#),
        format!(r#"{{"jsonrpc":"2.0","id":"x","method":"tools/call","params":{{{META},"name":"panics"}}}}"#),
        r#"{"jsonrpc":"2.0","id":8,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}},"clientInfo":{"name":"f","version":"1"}}}"#.to_owned(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_owned(),
        r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"delete","arguments":{"id":1}}}"#.to_owned(),
        r#"{"jsonrpc":"2.0","id":10,"method":"ping"}"#.to_owned(),
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":6}}"#.to_owned(),
        r#"{"jsonrpc":"2.0","id":11,"result":{"action":"accept","content":{"confirm":true}}}"#.to_owned(),
    ]
}

/// A stream of template messages, each possibly cut, spliced, or mutated.
fn stream() -> impl Strategy<Value = Vec<u8>> {
    let pieces = prop::collection::vec(
        (0..templates().len(), any::<u16>(), any::<u8>(), 0u8..4),
        1..24,
    );
    pieces.prop_map(|pieces| {
        let templates = templates();
        let mut out = Vec::new();
        for (index, at, byte, how) in pieces {
            let mut message = templates[index].clone().into_bytes();
            let at = usize::from(at) % message.len();
            match how {
                0 => {}
                1 => message[at] = byte,
                2 => message.truncate(at),
                _ => message.insert(at, byte),
            }
            out.extend(message);
            out.push(b'\n');
        }
        out
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn test_server_survives_arbitrary_bytes(data in prop::collection::vec(any::<u8>(), 0..2048)) {
        harness::check_server(server(), &data);
    }

    #[test]
    fn test_server_survives_mutated_messages(data in stream()) {
        harness::check_server(server(), &data);
    }

    #[test]
    fn test_schema_compiler_survives_arbitrary_bytes(data in prop::collection::vec(any::<u8>(), 0..512)) {
        harness::check_schema(&data);
    }

    #[test]
    fn test_schema_compiler_survives_json_shaped_input(
        schema in json_value(4),
        instance in json_value(4),
    ) {
        let mut data = schema.into_bytes();
        data.push(0);
        data.extend(instance.into_bytes());
        harness::check_schema(&data);
    }
}

/// Random JSON text drawn from schema-ish vocabulary, `depth` levels deep.
fn json_value(depth: u32) -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        Just("null".to_owned()),
        Just("true".to_owned()),
        Just("false".to_owned()),
        any::<i32>().prop_map(|n| n.to_string()),
        (any::<i16>(), 0u8..4).prop_map(|(n, e)| format!("{n}.5e-{e}")),
        prop::sample::select(vec![
            "\"object\"",
            "\"array\"",
            "\"string\"",
            "\"integer\"",
            "\"number\"",
            "\"#\"",
            "\"#/$defs/a\"",
            "\"a\"",
            "\"\"",
        ])
        .prop_map(str::to_owned),
    ];
    leaf.prop_recursive(depth, 64, 6, |inner| {
        let keys = prop::sample::select(vec![
            "type",
            "properties",
            "items",
            "prefixItems",
            "$ref",
            "$defs",
            "a",
            "allOf",
            "anyOf",
            "oneOf",
            "not",
            "if",
            "then",
            "else",
            "enum",
            "const",
            "required",
            "minimum",
            "maximum",
            "multipleOf",
            "minLength",
            "maxItems",
            "uniqueItems",
            "contains",
            "additionalProperties",
            "propertyNames",
            "dependentRequired",
            "$anchor",
            "x",
        ]);
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5)
                .prop_map(|items| format!("[{}]", items.join(","))),
            prop::collection::btree_map(keys, inner, 0..5).prop_map(|members| {
                let body: Vec<String> = members
                    .into_iter()
                    .map(|(k, v)| format!("\"{k}\":{v}"))
                    .collect();
                format!("{{{}}}", body.join(","))
            }),
        ]
    })
}
