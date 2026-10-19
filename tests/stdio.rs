//! End to end over the stdio transport: a real child process (the
//! `stdio_server` example) and in-memory pipes through `Server::serve`.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use mcp_io::{
    Implementation, Server, Tier, Tool, ToolResult,
    jsonx::{self, Value},
    rpc_io::Limits,
};

const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{"elicitation":{}}}"#;

/// The example's binary: the one this test run built next to it, or a fresh
/// build in a directory of its own (when only this test was built, or the
/// build used another target directory, as coverage runs do).
fn example() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let deps = exe.parent().unwrap();
    let name = format!("stdio_server{}", std::env::consts::EXE_SUFFIX);
    let beside = deps.parent().unwrap().join("examples").join(&name);
    if beside.exists() {
        return beside;
    }
    let target = deps.join("stdio-example");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--example", "stdio_server", "--target-dir"])
        .arg(&target)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap();
    assert!(status.success());
    target.join("debug").join("examples").join(name)
}

#[test]
fn test_child_process_speaks_newline_delimited_mcp_on_stdout_only() {
    let mut child = Command::new(example())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    let (lines_tx, lines) = mpsc::channel::<String>();
    let reader = thread::spawn(move || {
        for line in stdout.lines() {
            if lines_tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let mut ask = |line: String| {
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
        let reply = lines.recv_timeout(Duration::from_secs(30)).unwrap();
        jsonx::parse(&reply).unwrap().into_owned()
    };
    // Modern.
    let reply: Value<'static> = ask(format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{{{META}}}}}"#
    ));
    assert_eq!(
        reply["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "notes-example"
    );
    let reply = ask(format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{{META},"name":"notes_add","arguments":{{"text":"line one\nline two"}}}}}}"#
    ));
    assert_eq!(reply["result"]["content"][0]["text"], "stored note 1");
    let reply = ask(format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{{META},"name":"notes_list"}}}}"#
    ));
    assert_eq!(
        reply["result"]["content"][0]["text"], "line one\nline two",
        "a newline in content is escaped, never raw"
    );
    let reply = ask(format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{{META},"name":"notes_clear"}}}}"#
    ));
    assert_eq!(reply["result"]["resultType"], "input_required");
    // Legacy, on the same process.
    let reply = ask(r#"{"jsonrpc":"2.0","id":5,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"old","version":"1"}}}"#.to_owned());
    assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
    let reply = ask(r#"{"jsonrpc":"2.0","id":6,"method":"ping"}"#.to_owned());
    assert_eq!(reply["result"].to_string(), "{}");
    // Closing stdin is the shutdown signal: the server exits promptly.
    drop(stdin);
    let started = Instant::now();
    let status = child.wait().unwrap();
    assert!(status.success());
    assert!(started.elapsed() < Duration::from_secs(15));
    reader.join().unwrap();
    let mut log = String::new();
    stderr.read_to_string(&mut log).unwrap();
    assert!(
        log.contains("serving MCP on stdio"),
        "logs go to stderr: {log}"
    );
}

/// A `Read` fed from a channel, one chunk per message: how a pipe delivers.
struct Pipe(mpsc::Receiver<Vec<u8>>, Vec<u8>);

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.1.is_empty() {
            match self.0.recv() {
                Ok(chunk) => self.1 = chunk,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.1.len());
        buf[..n].copy_from_slice(&self.1[..n]);
        let _taken: Vec<u8> = self.1.drain(..n).collect();
        Ok(n)
    }
}

/// A `Write` that hands everything to a channel.
struct Sink(mpsc::Sender<Vec<u8>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.send(buf.to_vec());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn piped(
    server: Server,
) -> (
    mpsc::Sender<Vec<u8>>,
    mpsc::Receiver<Vec<u8>>,
    thread::JoinHandle<()>,
) {
    let (input, rx) = mpsc::channel();
    let (tx, output) = mpsc::channel();
    let serving = thread::spawn(move || {
        let end = server.serve(Pipe(rx, Vec::new()), Sink(tx));
        assert!(end.is_ok(), "{end:?}");
    });
    (input, output, serving)
}

fn read_lines(output: &mpsc::Receiver<Vec<u8>>, count: usize) -> Vec<String> {
    let mut bytes = Vec::new();
    while bytes.iter().filter(|&&b| b == b'\n').count() < count {
        bytes.extend(output.recv_timeout(Duration::from_secs(30)).unwrap());
    }
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn test_pipes_split_reads_unterminated_last_line_and_drain_on_eof() {
    let (started_tx, started) = mpsc::channel::<()>();
    let (release_tx, release) = mpsc::channel::<()>();
    let release = std::sync::Mutex::new(release);
    let server = Server::new(Implementation::new("pipe", "1"))
        .tool(Tool::new("slow", Tier::ReadOnly), move |_| {
            let _ = started_tx.send(());
            let _ = release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30));
            Ok(ToolResult::text("late but answered"))
        })
        .unwrap();
    let (input, output, serving) = piped(server);
    let request =
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{META}}}}}"#);
    // One message split across three reads, then two in one read.
    let (a, rest) = request.split_at(10);
    let (b, c) = rest.split_at(20);
    for part in [a, b, c, "\n"] {
        input.send(part.as_bytes().to_vec()).unwrap();
    }
    let slow = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{{META},"name":"slow"}}}}"#
    );
    let ping = r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#;
    input
        .send(format!("{slow}\n\r\n{ping}").into_bytes())
        .unwrap();
    started.recv_timeout(Duration::from_secs(30)).unwrap();
    // End of input with the slow call running and the ping unterminated;
    // the call is released after (or while) the server sees the end, and is
    // answered either way.
    drop(input);
    release_tx.send(()).unwrap();
    let lines = read_lines(&output, 3);
    serving.join().unwrap();
    let ids: Vec<i64> = lines
        .iter()
        .map(|l| jsonx::parse(l).unwrap()["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(
        ids.contains(&1) && ids.contains(&2) && ids.contains(&3),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("late but answered")));
}

#[test]
fn test_drain_timeout_bounds_shutdown() {
    let (release_tx, release) = mpsc::channel::<()>();
    let release = std::sync::Mutex::new(release);
    let server = Server::new(Implementation::new("pipe", "1"))
        .with_drain_timeout(Duration::from_millis(50))
        .tool(Tool::new("stuck", Tier::ReadOnly), move |_| {
            let _ = release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30));
            Ok(ToolResult::text("too late"))
        })
        .unwrap();
    let (input, output, serving) = piped(server);
    input
        .send(format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{{META},"name":"stuck"}}}}"#).into_bytes())
        .unwrap();
    drop(input);
    let started = Instant::now();
    serving.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "serve returned once the drain timed out"
    );
    drop(release_tx);
    assert!(
        output.recv_timeout(Duration::from_millis(300)).is_err(),
        "the abandoned call is not answered"
    );
}

#[test]
fn test_oversized_message_is_refused_and_serving_continues() {
    let server = Server::new(Implementation::new("pipe", "1"))
        .with_limits(Limits::mcp().with_max_message_bytes(256));
    let (input, output, serving) = piped(server);
    let huge = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"pad":"{}"}}}}"#,
        "x".repeat(1000)
    );
    input.send(format!("{huge}\n").into_bytes()).unwrap();
    input
        .send(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n".to_vec())
        .unwrap();
    let lines = read_lines(&output, 2);
    drop(input);
    serving.join().unwrap();
    assert!(lines[0].contains("-32600"), "{lines:?}");
    assert!(
        !lines[0].contains("\"id\""),
        "an oversized message's id is not read"
    );
    assert_eq!(lines[1], r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
}
