//! An in-process MCP client for the integration tests: drives a
//! `Connection` over an `mpsc` sink and reads what the server sends.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{
    sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    time::Duration,
};

use mcp_io::{
    Connection, Server,
    jsonx::{self, Value},
};

/// How long a test waits for a message it expects.
pub const WAIT: Duration = Duration::from_secs(20);

pub struct Client {
    pub connection: Connection<Sender<String>>,
    rx: Receiver<String>,
    /// Every message the server sent, in order, as text.
    pub log: Vec<String>,
}

impl Client {
    pub fn new(server: &Server) -> Self {
        let (tx, rx) = channel();
        let connection = server.connect(tx).unwrap();
        Self {
            connection,
            rx,
            log: Vec::new(),
        }
    }

    /// Sends one message's text.
    pub fn send(&mut self, text: &str) {
        let _flow = self.connection.handle(text.as_bytes()).unwrap();
    }

    /// The next message the server sends, parsed.
    pub fn recv(&mut self) -> Value<'static> {
        let text = self.rx.recv_timeout(WAIT).expect("the server sent nothing");
        self.log.push(text.clone());
        jsonx::parse(&text).unwrap().into_owned()
    }

    /// The next message, or `None` if none arrives within `wait`.
    pub fn try_recv(&mut self, wait: Duration) -> Option<Value<'static>> {
        match self.rx.recv_timeout(wait) {
            Ok(text) => {
                self.log.push(text.clone());
                Some(jsonx::parse(&text).unwrap().into_owned())
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Sends a request and returns its response, keeping notifications that
    /// come first in `before`.
    pub fn request(&mut self, text: &str) -> Value<'static> {
        self.send(text);
        loop {
            let message = self.recv();
            if message.get("id").is_some() || message.get("method").is_none() {
                return message;
            }
        }
    }
}

/// The `_meta` of a 2026-07-28 request with `capabilities`.
pub fn meta_with(capabilities: &str) -> String {
    format!(
        r#""_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{capabilities},"io.modelcontextprotocol/clientInfo":{{"name":"test-client","version":"1.0.0"}}}}"#
    )
}

/// The `_meta` of a 2026-07-28 request that declares no capabilities.
pub fn meta() -> String {
    meta_with("{}")
}

/// A 2026-07-28 request: `params` is the inside of the params object
/// (without braces), `_meta` is added.
pub fn modern(id: u64, method: &str, params: &str) -> String {
    let comma = if params.is_empty() { "" } else { "," };
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{{}{comma}{params}}}}}"#,
        meta()
    )
}

/// A 2026-07-28 `tools/call`.
pub fn call(id: u64, tool: &str, arguments: &str) -> String {
    modern(
        id,
        "tools/call",
        &format!(r#""name":"{tool}","arguments":{arguments}"#),
    )
}

/// A legacy `initialize` asking for `version`.
pub fn initialize(id: u64, version: &str, capabilities: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"initialize","params":{{"protocolVersion":"{version}","capabilities":{capabilities},"clientInfo":{{"name":"legacy-client","version":"0.9.0"}}}}}}"#
    )
}

pub const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

/// Opens a legacy 2025-11-25 session on `client`.
pub fn open_legacy(client: &mut Client, capabilities: &str) {
    let reply = client.request(&initialize(0, "2025-11-25", capabilities));
    assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
    client.send(INITIALIZED);
}

/// The error code of a response, if it is an error.
pub fn code(response: &Value<'_>) -> Option<i64> {
    response.get("error").and_then(|e| e["code"].as_i64())
}

/// The first text block of a tool result.
pub fn text(response: &Value<'_>) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}
