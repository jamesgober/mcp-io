//! The tool registry: one ordered set of tools per server, shared by every
//! connection, with pagination and list-changed notifications.
//!
//! Tools are kept sorted by name, so `tools/list` is deterministic (MCP
//! SHOULD) and a cursor can be the name the next page starts after: stable
//! when other tools are added or removed between pages. The set is the same
//! for every connection (MCP: it "MUST NOT vary per-connection").

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, PoisonError, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    RegisterError, Schema, SchemaLimits, Tier, Tool, ToolCall, ToolError, ToolResult,
    jsonx::Value,
    rpc_io::{CancelToken, Notifier, Peer},
};

/// What a registered tool runs: `Fn(&ToolCall) -> Result<ToolResult, ToolError>`.
pub(crate) type Handler = Arc<dyn Fn(&ToolCall<'_>) -> Result<ToolResult, ToolError> + Send + Sync>;

/// A registered tool.
pub(crate) struct Entry {
    pub(crate) name: Box<str>,
    pub(crate) tier: Tier,
    pub(crate) confirmation: Option<String>,
    /// The `Tool` object of `tools/list`, rendered once.
    pub(crate) definition: String,
    /// The same for legacy sessions (no non-object output schema).
    pub(crate) legacy_definition: String,
    pub(crate) input: Schema,
    pub(crate) output: Option<Schema>,
    pub(crate) handler: Handler,
    /// Which registration this is: confirmations are bound to it, so a tool
    /// registered again under the same name does not inherit them.
    pub(crate) generation: u64,
}

/// A connection to tell when the tool list changes.
struct Watcher {
    connection: u64,
    peer: Peer,
    /// For a `subscriptions/listen` stream; `None` for a legacy session.
    subscription: Option<Stream>,
}

/// A `subscriptions/listen` stream that asked for list changes.
#[derive(Clone)]
pub(crate) struct Stream {
    /// The listen request's id, as JSON text (the `subscriptionId`).
    pub(crate) id: String,
    /// Sends for the listen request only while it is open: rpc-io refuses
    /// once it is answered or cancelled, under the lock its response is
    /// written under, so no list change can follow the stream's end.
    pub(crate) notifier: Notifier,
    /// Set when the client cancels the stream, to forget the watcher.
    pub(crate) token: CancelToken,
}

pub(crate) struct Registry {
    tools: RwLock<BTreeMap<Box<str>, Arc<Entry>>>,
    watchers: Mutex<Vec<Watcher>>,
    limits: RwLock<SchemaLimits>,
}

/// Registration numbers, unique within the process.
static GENERATIONS: AtomicU64 = AtomicU64::new(0);

/// The cursor prefix; what follows is the last name of the previous page.
const CURSOR_PREFIX: &str = "after:";

impl Registry {
    pub(crate) fn new() -> Self {
        Self {
            tools: RwLock::new(BTreeMap::new()),
            watchers: Mutex::new(Vec::new()),
            limits: RwLock::new(SchemaLimits::new()),
        }
    }

    pub(crate) fn set_limits(&self, limits: SchemaLimits) {
        *self.limits.write().unwrap_or_else(PoisonError::into_inner) = limits;
    }

    pub(crate) fn add(&self, tool: Tool, handler: Handler) -> Result<(), RegisterError> {
        tool.check()?;
        let limits = *self.limits.read().unwrap_or_else(PoisonError::into_inner);
        let input = match &tool.input_schema {
            Some(schema) => Schema::compile_with(schema, limits),
            None => Schema::compile_with(&no_arguments(), limits),
        }
        .map_err(|error| RegisterError::Schema {
            tool: tool.name.clone(),
            which: "inputSchema",
            error,
        })?;
        let output = tool
            .output_schema
            .as_ref()
            .map(|schema| Schema::compile_with(schema, limits))
            .transpose()
            .map_err(|error| RegisterError::Schema {
                tool: tool.name.clone(),
                which: "outputSchema",
                error,
            })?;
        let entry = Arc::new(Entry {
            name: tool.name.as_str().into(),
            tier: tool.tier,
            definition: tool.definition_json(false),
            legacy_definition: tool.definition_json(true),
            confirmation: tool.confirmation,
            input,
            output,
            handler,
            generation: GENERATIONS.fetch_add(1, Ordering::Relaxed),
        });
        {
            let mut tools = self.tools.write().unwrap_or_else(PoisonError::into_inner);
            if tools.contains_key(&*entry.name) {
                return Err(RegisterError::Duplicate { name: tool.name });
            }
            let _none = tools.insert(entry.name.clone(), entry);
        }
        self.changed();
        Ok(())
    }

    pub(crate) fn remove(&self, name: &str) -> bool {
        let removed = self
            .tools
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name)
            .is_some();
        if removed {
            self.changed();
        }
        removed
    }

    pub(crate) fn get(&self, name: &str) -> Option<Arc<Entry>> {
        self.tools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.tools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .map(|name| name.to_string())
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.tools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// One page of tool definitions (JSON text, comma-separated) after
    /// `cursor`, and the cursor of the next page if there is one. `None` for
    /// a cursor this server did not issue.
    pub(crate) fn page(
        &self,
        cursor: Option<&str>,
        size: usize,
        legacy: bool,
    ) -> Option<(String, Option<String>)> {
        let after = match cursor {
            None => None,
            Some(cursor) => {
                let name = cursor.strip_prefix(CURSOR_PREFIX)?;
                crate::tool::check_name(name).ok()?;
                Some(name)
            }
        };
        let tools = self.tools.read().unwrap_or_else(PoisonError::into_inner);
        let mut range: Box<dyn Iterator<Item = (&Box<str>, &Arc<Entry>)>> = match after {
            None => Box::new(tools.iter()),
            Some(name) => Box::new(tools.range::<str, _>((
                core::ops::Bound::Excluded(name),
                core::ops::Bound::Unbounded,
            ))),
        };
        let mut out = String::new();
        let mut last: Option<&str> = None;
        for (i, (name, entry)) in range.by_ref().take(size.max(1)).enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(if legacy {
                &entry.legacy_definition
            } else {
                &entry.definition
            });
            last = Some(name);
        }
        let next = match (range.next(), last) {
            (Some(_), Some(last)) => Some(format!("{CURSOR_PREFIX}{last}")),
            _ => None,
        };
        Some((out, next))
    }

    /// Tells `peer` about changes from now on: as a legacy session's plain
    /// notification, or on the `subscriptions/listen` stream `subscription`.
    pub(crate) fn watch(&self, connection: u64, peer: Peer, subscription: Option<Stream>) {
        let mut watchers = self.watchers.lock().unwrap_or_else(PoisonError::into_inner);
        // Drop the dead first, so the list holds only live streams (which
        // each connection caps) however often a client opens and cancels.
        watchers.retain(Watcher::is_live);
        watchers.push(Watcher {
            connection,
            peer,
            subscription,
        });
    }

    /// Forgets every watcher of `connection` (it closed).
    pub(crate) fn unwatch(&self, connection: u64) {
        self.watchers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|w| w.connection != connection);
    }

    /// Sends `notifications/tools/list_changed` to every live watcher.
    fn changed(&self) {
        let targets: Vec<(Peer, Option<Stream>)> = {
            let mut watchers = self.watchers.lock().unwrap_or_else(PoisonError::into_inner);
            watchers.retain(Watcher::is_live);
            watchers
                .iter()
                .map(|w| (w.peer.clone(), w.subscription.clone()))
                .collect()
        };
        for (peer, subscription) in targets {
            let sent = match subscription {
                Some(stream) => crate::json::raw(format!(
                    "{{\"_meta\":{{\"{}\":{}}}}}",
                    crate::protocol::meta::SUBSCRIPTION_ID,
                    stream.id
                ))
                .map_err(|_| ())
                .and_then(|params| {
                    // Refused (and nothing sent) once the stream ended.
                    stream
                        .notifier
                        .notify("notifications/tools/list_changed", params)
                        .map_err(|_| ())
                }),
                None => peer
                    .notify("notifications/tools/list_changed", ())
                    .map_err(|_| ()),
            };
            // A connection that closed, or a stream that ended, between the
            // snapshot and the send has no one left to tell; it is dropped
            // from the list next time.
            let _closed = sent;
        }
    }
}

impl Watcher {
    /// Whether the connection is open and, for a stream, not cancelled.
    fn is_live(&self) -> bool {
        !self.peer.is_closed()
            && self
                .subscription
                .as_ref()
                .is_none_or(|stream| !stream.token.is_cancelled())
    }
}

fn no_arguments() -> Value<'static> {
    // A constant that always parses.
    crate::jsonx::parse(crate::tool::NO_ARGUMENTS)
        .map(Value::into_owned)
        .unwrap_or(Value::Bool(true))
}

/// The server's tools, to add and remove while it serves. Every connection
/// sees the change; clients that asked for list changes (a 2026-07-28
/// `subscriptions/listen` stream with `toolsListChanged`, or an initialized
/// legacy session) are sent `notifications/tools/list_changed`.
///
/// `Clone + Send + Sync`; get it from [`Server::tools`](crate::Server::tools).
///
/// # Examples
///
/// ```
/// use mcp_io::{Implementation, Server, Tier, Tool, ToolResult};
///
/// let server = Server::new(Implementation::new("demo", "1.0.0"));
/// let tools = server.tools();
/// tools.add(Tool::new("hello", Tier::ReadOnly), |_| Ok(ToolResult::text("hi")))?;
/// assert_eq!(tools.names(), ["hello"]);
/// assert!(tools.remove("hello"));
/// assert!(tools.is_empty());
/// # Ok::<(), mcp_io::RegisterError>(())
/// ```
#[derive(Clone)]
pub struct Tools(pub(crate) Arc<Registry>);

impl Tools {
    /// Registers `tool` with `handler`.
    ///
    /// # Errors
    ///
    /// [`RegisterError`]: an invalid name, a name taken already, an input
    /// schema whose root is not `"type": "object"`, or a schema that does
    /// not compile.
    ///
    /// # Examples
    ///
    /// See [`Tools`].
    pub fn add<F>(&self, tool: Tool, handler: F) -> Result<(), RegisterError>
    where
        F: Fn(&ToolCall<'_>) -> Result<ToolResult, ToolError> + Send + Sync + 'static,
    {
        self.0.add(tool, Arc::new(handler))
    }

    /// Registers a typed tool: its arguments are decoded into `A` (after the
    /// input schema has passed them), and the handler's `R` becomes the
    /// result's structured content, with a text copy. See
    /// [`Server::tool_json`](crate::Server::tool_json).
    ///
    /// # Errors
    ///
    /// As [`add`](Self::add).
    ///
    /// # Examples
    ///
    /// See [`Server::tool_json`](crate::Server::tool_json).
    pub fn add_json<A, R, F>(&self, tool: Tool, handler: F) -> Result<(), RegisterError>
    where
        A: for<'de> crate::jsonx::FromJson<'de>,
        R: crate::jsonx::ToJson,
        F: Fn(&ToolCall<'_>, A) -> Result<R, ToolError> + Send + Sync + 'static,
    {
        self.0.add(tool, typed(handler))
    }

    /// Removes the tool named `name`; `false` if there was none. Calls
    /// already running finish.
    ///
    /// # Examples
    ///
    /// See [`Tools`].
    pub fn remove(&self, name: &str) -> bool {
        self.0.remove(name)
    }

    /// Whether a tool named `name` is registered.
    ///
    /// # Examples
    ///
    /// ```
    /// let server = mcp_io::Server::new(mcp_io::Implementation::new("demo", "1.0.0"));
    /// assert!(!server.tools().contains("x"));
    /// ```
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.0.get(name).is_some()
    }

    /// The registered names, sorted (the order of `tools/list`).
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.0.names()
    }

    /// How many tools are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no tool is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Wraps a typed handler: decode the arguments, run, encode the result.
pub(crate) fn typed<A, R, F>(handler: F) -> Handler
where
    A: for<'de> crate::jsonx::FromJson<'de>,
    R: crate::jsonx::ToJson,
    F: Fn(&ToolCall<'_>, A) -> Result<R, ToolError> + Send + Sync + 'static,
{
    Arc::new(move |call: &ToolCall<'_>| {
        let arguments = call.decode::<A>()?;
        handler(call, arguments).map(|result| ToolResult::encode(&result))
    })
}

impl core::fmt::Debug for Tools {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tools")
            .field("names", &self.names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(names: &[&str]) -> Registry {
        let registry = Registry::new();
        for name in names {
            let added = registry.add(
                Tool::new(*name, Tier::ReadOnly),
                Arc::new(|_: &ToolCall<'_>| Ok(ToolResult::new())),
            );
            assert!(added.is_ok());
        }
        registry
    }

    fn names_of(page: &str) -> Vec<String> {
        let list = crate::jsonx::parse(&format!("[{page}]"))
            .ok()
            .map(Value::into_owned);
        list.and_then(|l| {
            l.as_array().map(|a| {
                a.iter()
                    .filter_map(|t| t["name"].as_str().map(str::to_owned))
                    .collect()
            })
        })
        .unwrap_or_default()
    }

    #[test]
    fn test_pages_are_sorted_and_cover_everything_once() {
        let r = registry(&["c", "a", "e", "b", "d"]);
        let (page, next) = r.page(None, 2, false).unwrap_or_default();
        assert_eq!(names_of(&page), ["a", "b"]);
        assert_eq!(next.as_deref(), Some("after:b"));
        let (page, next) = r.page(next.as_deref(), 2, false).unwrap_or_default();
        assert_eq!(names_of(&page), ["c", "d"]);
        let (page, next) = r.page(next.as_deref(), 2, false).unwrap_or_default();
        assert_eq!(names_of(&page), ["e"]);
        assert_eq!(next, None);
    }

    #[test]
    fn test_cursor_survives_changes_between_pages() {
        let r = registry(&["a", "b", "c"]);
        let (_, next) = r.page(None, 1, false).unwrap_or_default();
        assert!(r.remove("a") && r.remove("b"));
        let (page, _) = r.page(next.as_deref(), 1, false).unwrap_or_default();
        assert_eq!(names_of(&page), ["c"]);
    }

    #[test]
    fn test_foreign_cursors_are_refused() {
        let r = registry(&["a"]);
        for cursor in ["", "a", "after:", "after:bad name", "before:a"] {
            assert!(r.page(Some(cursor), 10, false).is_none(), "{cursor}");
        }
        assert!(
            r.page(Some("after:zzz"), 10, false)
                .is_some_and(|(page, next)| page.is_empty() && next.is_none())
        );
    }

    #[test]
    fn test_watchers_of_ended_streams_are_dropped_when_another_starts() {
        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        let router = rpc_io::Router::new()
            .deferred("listen", |held: &mut Vec<rpc_io::Responder>, _, _, r| {
                held.push(r)
            });
        let mut endpoint = rpc_io::Endpoint::new(tx, router, Vec::new());
        for i in 0..100 {
            let request = format!(r#"{{"jsonrpc":"2.0","id":{i},"method":"listen"}}"#);
            assert!(endpoint.handle(request.as_bytes()).is_ok());
        }
        let r = registry(&[]);
        for responder in endpoint.state() {
            let token = responder.token();
            let stream = Stream {
                id: responder.id().to_string(),
                notifier: responder.notifier(),
                token: token.clone(),
            };
            r.watch(1, endpoint.peer(), Some(stream));
            token.cancel();
        }
        r.watch(1, endpoint.peer(), None);
        let live = r.watchers.lock().map(|w| w.len()).unwrap_or(0);
        assert_eq!(live, 1);
    }

    #[test]
    fn test_duplicates_and_bad_schemas_are_refused() {
        let r = registry(&["a"]);
        let handler: Handler = Arc::new(|_: &ToolCall<'_>| Ok(ToolResult::new()));
        assert!(matches!(
            r.add(Tool::new("a", Tier::ReadOnly), handler.clone()),
            Err(RegisterError::Duplicate { .. })
        ));
        let bad_in = Tool::new("b", Tier::ReadOnly).input_schema(
            crate::jsonx::json!({ "type": "object", "properties": { "x": { "pattern": "." } } }),
        );
        assert!(matches!(
            r.add(bad_in, handler.clone()),
            Err(RegisterError::Schema {
                which: "inputSchema",
                ..
            })
        ));
        let bad_out = Tool::new("c", Tier::ReadOnly).output_schema(crate::jsonx::json!(1));
        assert!(matches!(
            r.add(bad_out, handler),
            Err(RegisterError::Schema {
                which: "outputSchema",
                ..
            })
        ));
        assert_eq!(r.len(), 1);
    }
}
