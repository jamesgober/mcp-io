//! Property tests: the validator's numeric and equality semantics against
//! simple reference implementations, pagination, and the one-answer-per-
//! request invariant.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{Client, call, modern};
use mcp_io::{
    Implementation, Schema, Server, Tier, Tool, ToolResult,
    jsonx::{self, Value, json},
};
use proptest::prelude::*;

/// The reference model: JSON values compared by meaning.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Reference {
    Null,
    Bool(bool),
    Number(i64),
    String(String),
    Array(Vec<Reference>),
    Object(BTreeMap<String, Reference>),
}

/// Small JSON values with their meaning: integers in several spellings,
/// short strings, booleans, null, and arrays and objects of them (objects
/// written in either member order).
fn value() -> impl Strategy<Value = (String, Reference)> {
    let leaf = prop_oneof![
        (-3i64..3, 0u8..3).prop_map(|(n, spelling)| {
            let text = match spelling {
                0 => n.to_string(),
                1 => format!("{n}.0"),
                _ => format!("{}e-1", n * 10),
            };
            (text, Reference::Number(n))
        }),
        prop::sample::select(vec!["a", "b", "1"])
            .prop_map(|s| (format!("\"{s}\""), Reference::String(s.to_owned()))),
        any::<bool>().prop_map(|b| (b.to_string(), Reference::Bool(b))),
        Just(("null".to_owned(), Reference::Null)),
    ];
    leaf.prop_recursive(3, 24, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(|items| {
                let text = items
                    .iter()
                    .map(|(t, _)| t.clone())
                    .collect::<Vec<_>>()
                    .join(",");
                (
                    format!("[{text}]"),
                    Reference::Array(items.into_iter().map(|(_, r)| r).collect()),
                )
            }),
            (
                prop::collection::btree_map(prop::sample::select(vec!["x", "y", "z"]), inner, 0..3),
                any::<bool>()
            )
                .prop_map(|(members, reversed)| {
                    let mut pairs: Vec<_> = members.into_iter().collect();
                    let reference = Reference::Object(
                        pairs
                            .iter()
                            .map(|(k, (_, r))| ((*k).to_owned(), r.clone()))
                            .collect(),
                    );
                    if reversed {
                        pairs.reverse();
                    }
                    let text = pairs
                        .iter()
                        .map(|(k, (t, _))| format!("\"{k}\":{t}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    (format!("{{{text}}}"), reference)
                }),
        ]
    })
}

/// The reference `uniqueItems`: pairwise, quadratic, obviously right.
fn naive_unique(items: &[Reference]) -> bool {
    for (i, a) in items.iter().enumerate() {
        for b in &items[i + 1..] {
            if a == b {
                return false;
            }
        }
    }
    true
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn test_unique_items_matches_the_pairwise_reference(items in prop::collection::vec(value(), 0..6)) {
        let text = format!("[{}]", items.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>().join(","));
        let references: Vec<Reference> = items.into_iter().map(|(_, r)| r).collect();
        let schema = Schema::compile(&json!({ "uniqueItems": true })).unwrap();
        let instance = jsonx::parse(&text).unwrap();
        prop_assert_eq!(schema.is_valid(&instance), naive_unique(&references), "{}", text);
    }

    #[test]
    fn test_const_equality_matches_the_reference((a, ra) in value(), (b, rb) in value()) {
        let schema = Schema::compile(&jsonx::parse(&format!(r#"{{"const":{a}}}"#)).unwrap()).unwrap();
        prop_assert_eq!(schema.is_valid(&jsonx::parse(&b).unwrap()), ra == rb, "{} vs {}", a, b);
    }

    #[test]
    fn test_bounds_match_integer_comparison(bound in any::<i64>(), x in any::<i64>(), scale in 0usize..4) {
        // `x` written as x * 10^scale e-scale: the same value, other text.
        let x_text = if scale == 0 { x.to_string() } else { format!("{x}{}e-{scale}", "0".repeat(scale)) };
        let instance = jsonx::parse(&x_text).unwrap();
        let min = Schema::compile(&json!({ "minimum": bound })).unwrap();
        let exclusive = Schema::compile(&json!({ "exclusiveMaximum": bound })).unwrap();
        prop_assert_eq!(min.is_valid(&instance), x >= bound);
        prop_assert_eq!(exclusive.is_valid(&instance), x < bound);
    }

    #[test]
    fn test_multiple_of_matches_integer_remainder(x in -1_000_000i64..1_000_000, m in 1i64..1000, shift in 0u32..3) {
        // Divide both by 10^shift: the quotient, and so the answer, stays.
        let scaled = |n: i64| if shift == 0 { n.to_string() } else { format!("{n}e-{shift}") };
        let schema = Schema::compile(&jsonx::parse(&format!(r#"{{"multipleOf":{}}}"#, scaled(m))).unwrap()).unwrap();
        prop_assert_eq!(schema.is_valid(&jsonx::parse(&scaled(x)).unwrap()), x % m == 0);
    }

}

proptest! {
    // Each case starts a server (and, below, its worker threads).
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn test_pages_cover_every_tool_once_in_order(
        names in prop::collection::btree_set("[a-z]{1,6}", 0..30),
        page in 1usize..7,
    ) {
        let server = Server::new(Implementation::new("p", "1")).with_page_size(page);
        for name in &names {
            server.tools().add(Tool::new(name.clone(), Tier::ReadOnly), |_| Ok(ToolResult::new())).unwrap();
        }
        let mut client = Client::new(&server);
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        for id in 1..100 {
            let params = cursor.as_ref().map(|c| format!(r#""cursor":"{c}""#)).unwrap_or_default();
            let reply = client.request(&modern(id, "tools/list", &params));
            let tools = reply["result"]["tools"].as_array().unwrap();
            prop_assert!(tools.len() <= page);
            seen.extend(tools.iter().map(|t| t["name"].as_str().unwrap().to_owned()));
            match reply["result"].get("nextCursor").and_then(Value::as_str) {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
        }
        prop_assert_eq!(seen, names.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn test_every_request_is_answered_exactly_once(script in prop::collection::vec(0u8..8, 1..40)) {
        let server = Server::new(Implementation::new("p", "1"))
            .with_workers(2)
            .tool(
                Tool::new("echo", Tier::ReadOnly)
                    .input_schema(json!({ "type": "object", "properties": { "n": { "type": "integer" } } })),
                |call| Ok(ToolResult::text(call.arguments().to_string())),
            )
            .unwrap();
        let mut client = Client::new(&server);
        let mut expected = BTreeSet::new();
        for (i, step) in script.iter().enumerate() {
            let id = i as u64 + 1;
            let line = match step {
                0 => call(id, "echo", r#"{"n":1}"#),
                1 => call(id, "echo", r#"{"n":"x"}"#),
                2 => call(id, "missing", "{}"),
                3 => modern(id, "tools/list", ""),
                4 => modern(id, "server/discover", ""),
                5 => modern(id, "no/such/method", ""),
                6 => format!(
                    r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{"requestId":{}}}}}"#,
                    id + 1000
                ),
                _ => r#"{"jsonrpc":"2.0","method":"notifications/unknown"}"#.to_owned(),
            };
            if *step < 6 {
                let _new = expected.insert(i64::try_from(id).unwrap());
            }
            client.send(&line);
        }
        prop_assert!(client.connection.wait_idle(common::WAIT));
        let mut answered = Vec::new();
        while let Some(message) = client.try_recv(std::time::Duration::from_millis(20)) {
            if let Some(id) = message.get("id").and_then(Value::as_i64) {
                answered.push(id);
            }
        }
        answered.sort_unstable();
        prop_assert_eq!(answered, expected.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn test_a_confirmation_shows_every_argument_or_refuses(
        members in prop::collection::btree_map("[a-z]{1,8}", "[ -~]{0,300}", 1..8),
    ) {
        let server = Server::new(Implementation::new("p", "1"))
            .with_workers(0)
            .with_max_confirmed_arguments(512)
            .tool(Tool::new("ship", Tier::Publish).input_schema(json!({ "type": "object" })), |_| Ok(ToolResult::new()))
            .unwrap();
        let mut client = Client::new(&server);
        let object: mcp_io::jsonx::Object<'_> = members.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect();
        let arguments = Value::Object(object);
        let reply = client.request(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{"elicitation":{{}}}}}},"name":"ship","arguments":{arguments}}}}}"#
        ));
        let result = &reply["result"];
        if result["resultType"] == "input_required" {
            // Asked: the question holds the arguments, whole and parseable.
            let message = result["inputRequests"]["confirm"]["params"]["message"].as_str().unwrap();
            let shown = message.rsplit_once("Arguments: ").map(|(_, a)| a).unwrap();
            let shown = mcp_io::jsonx::parse(shown).unwrap();
            for (key, value) in &members {
                prop_assert_eq!(shown[key.as_str()].as_str(), Some(value.as_str()));
            }
            prop_assert_eq!(shown.as_object().map(|o| o.len()), Some(members.len()));
        } else {
            // Not asked: refused, as too large to show.
            prop_assert_eq!(result["isError"].as_bool(), Some(true));
            prop_assert!(arguments.to_string().len() > 400);
        }
    }
}
