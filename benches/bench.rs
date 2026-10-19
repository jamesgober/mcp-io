//! Criterion benchmarks for the hot paths: schema validation, `tools/list`
//! pages over large registries, and a whole `tools/call` through a
//! connection.
//!
//! ```text
//! cargo bench --bench bench
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{hint::black_box, sync::mpsc};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mcp_io::{
    Implementation, Schema, Server, Tier, Tool, ToolResult,
    jsonx::{self, Value, json},
};

const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;

fn input_schema() -> Value<'static> {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "minLength": 1, "maxLength": 256 },
            "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
            "tags": { "type": "array", "items": { "type": "string" }, "uniqueItems": true, "maxItems": 32 },
            "sort": { "enum": ["relevance", "date", "title"] },
            "range": {
                "type": "object",
                "properties": { "from": { "type": "string" }, "to": { "type": "string" } },
                "additionalProperties": false
            }
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

fn bench_validate(c: &mut Criterion) {
    let schema = Schema::compile(&input_schema()).unwrap();
    let mut group = c.benchmark_group("validate");
    let typical = jsonx::parse(r#"{"query":"rust mcp server","limit":20,"tags":["a","b","c"],"sort":"date","range":{"from":"2026-01-01","to":"2026-10-01"}}"#).unwrap();
    let _ = group.bench_function("typical_arguments", |b| {
        b.iter(|| schema.is_valid(black_box(&typical)))
    });
    for n in [1_000usize, 100_000] {
        let tags: Vec<Value<'static>> = (0..n).map(|i| Value::from(format!("tag{i}"))).collect();
        let array_schema = Schema::compile(
            &json!({ "type": "array", "items": { "type": "string" }, "uniqueItems": true }),
        )
        .unwrap();
        let instance = Value::from(tags);
        let _ = group.throughput(Throughput::Elements(n as u64));
        let _ = group.bench_with_input(
            BenchmarkId::new("unique_strings", n),
            &instance,
            |b, instance| {
                b.iter(|| array_schema.is_valid(black_box(instance)));
            },
        );
    }
    group.finish();
}

fn server_with(tools: usize) -> Server {
    let server = Server::new(Implementation::new("bench", "1.0.0")).with_workers(0);
    for i in 0..tools {
        server
            .tools()
            .add(
                Tool::new(format!("tool_{i:06}"), Tier::ReadOnly)
                    .description("A tool that searches the notes")
                    .input_schema(input_schema()),
                |_| Ok(ToolResult::text("ok")),
            )
            .unwrap();
    }
    server
}

fn bench_list(c: &mut Criterion) {
    let mut group = c.benchmark_group("tools_list");
    for tools in [1_000usize, 100_000] {
        let server = server_with(tools);
        let (tx, rx) = mpsc::channel::<String>();
        let mut connection = server.connect(tx).unwrap();
        let first =
            format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{META}}}}}"#);
        let middle = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{{META},"cursor":"after:tool_{:06}"}}}}"#,
            tools / 2
        );
        let _ = group.bench_function(BenchmarkId::new("first_page_of_100", tools), |b| {
            b.iter(|| {
                connection.handle(black_box(first.as_bytes())).unwrap();
                rx.recv().unwrap()
            });
        });
        let _ = group.bench_function(BenchmarkId::new("middle_page_of_100", tools), |b| {
            b.iter(|| {
                connection.handle(black_box(middle.as_bytes())).unwrap();
                rx.recv().unwrap()
            });
        });
    }
    group.finish();
}

fn bench_call(c: &mut Criterion) {
    let mut group = c.benchmark_group("tools_call");
    for tools in [1usize, 100_000] {
        let server = server_with(tools);
        let (tx, rx) = mpsc::channel::<String>();
        let mut connection = server.connect(tx).unwrap();
        let call = format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{{META},"name":"tool_000000","arguments":{{"query":"rust","limit":5,"tags":["x","y"]}}}}}}"#
        );
        let _ = group.bench_function(BenchmarkId::new("inline_round_trip", tools), |b| {
            b.iter(|| {
                connection.handle(black_box(call.as_bytes())).unwrap();
                rx.recv().unwrap()
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_validate, bench_list, bench_call);
criterion_main!(benches);
