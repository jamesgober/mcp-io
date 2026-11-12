//! A call that needs confirmation and is too large to show is refused
//! without building its shown form in full: the memory it costs is bounded
//! by the message, not by several times it.
//!
//! Its own test binary: the allocator below counts every allocation in the
//! process, so nothing else may run beside it.

#![allow(clippy::unwrap_used)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use mcp_io::{Implementation, Server, Tier, Tool, ToolResult, jsonx::json};

/// The system allocator, keeping the bytes in use and their peak.
struct Counting;

static IN_USE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is passed to the system allocator unchanged; only the
// counters are added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc` is passed on as is.
        let block = unsafe { System.alloc(layout) };
        if !block.is_null() {
            let now = IN_USE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            let _previous = PEAK.fetch_max(now, Ordering::Relaxed);
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract for `dealloc` is passed on as is.
        unsafe { System.dealloc(block, layout) };
        let _previous = IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn test_an_oversized_confirmation_is_refused_within_bounded_memory() {
    let server = Server::new(Implementation::new("memory-test", "1.0.0"))
        .with_workers(0)
        .tool(
            Tool::new("delete", Tier::Destructive).input_schema(json!({
                "type": "object",
                "properties": { "target": { "type": "string" } }
            })),
            |_| Ok(ToolResult::text("deleted")),
        )
        .unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    let mut connection = server.connect(tx).unwrap();
    // 16 MiB of UTF-8 in one string, each 4-byte character shown as a
    // 12-byte escape: 48 MiB as shown.
    let target = "\u{1F600}".repeat(4 << 20);
    let message = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{"elicitation":{{"form":{{}}}}}}}},"name":"delete","arguments":{{"target":"{target}"}}}}}}"#
    );
    drop(target);
    let base = IN_USE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    connection.handle(message.as_bytes()).unwrap();
    let reply = rx.recv_timeout(Duration::from_secs(60)).unwrap();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    assert!(
        reply.contains("too large to show in full") && reply.contains("more than 4096 bytes"),
        "{}",
        &reply[..reply.len().min(400)]
    );
    // The params are copied once for the worker; the shown form is never
    // built past the 4 KiB limit (it used to cost the copy, the compact
    // text, and the escaped text: nine times the message).
    assert!(
        peak < 2 * message.len(),
        "the call held {} MiB above the {} MiB message",
        peak >> 20,
        message.len() >> 20
    );
}
