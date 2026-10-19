//! The server: its identity, settings, tools and policy, and the connections
//! it serves.

use std::{
    io::{Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::{
    AuditRecord, Ceiling, Error, Policy, RegisterError, SchemaLimits, Tool, ToolCall, ToolError,
    ToolResult, Tools,
    json::Obj,
    permission::Ledger,
    protocol::Era,
    registry::Registry,
    rpc_io::{Config, End, Endpoint, Flow, Framing, Limits, Next, Peer, Queued, Sink, Stats},
    session::{self, Lifecycle, Session},
};

/// The server's self-description: MCP's `Implementation`, sent as
/// `serverInfo` (in every 2026-07-28 result's `_meta`, and in a legacy
/// `initialize` result).
///
/// # Examples
///
/// ```
/// use mcp_io::Implementation;
///
/// let info = Implementation::new("notes-server", "1.4.0")
///     .title("Notes")
///     .description("Search and edit the team's notes")
///     .website_url("https://example.com/notes");
/// assert_eq!(
///     info.to_json(),
///     r#"{"name":"notes-server","version":"1.4.0","title":"Notes","description":"Search and edit the team's notes","websiteUrl":"https://example.com/notes"}"#,
/// );
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Implementation {
    name: String,
    version: String,
    title: Option<String>,
    description: Option<String>,
    website_url: Option<String>,
}

impl Implementation {
    /// A server called `name` at `version`.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::Implementation::new("s", "1").name(), "s");
    /// ```
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            title: None,
            description: None,
            website_url: None,
        }
    }

    /// A display name.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// What the server does.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The server's website.
    #[must_use]
    pub fn website_url(mut self, url: impl Into<String>) -> Self {
        self.website_url = Some(url.into());
        self
    }

    /// The name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// As MCP JSON text.
    ///
    /// # Examples
    ///
    /// See [`Implementation`].
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut o = Obj::new();
        let _ = o
            .str("name", &self.name)
            .str("version", &self.version)
            .opt_str("title", self.title.as_deref())
            .opt_str("description", self.description.as_deref())
            .opt_str("websiteUrl", self.website_url.as_deref());
        o.finish()
    }
}

/// Who may cache a list result (MCP's `cacheScope`).
///
/// # Examples
///
/// ```
/// assert_eq!(mcp_io::CacheScope::Public.as_str(), "public");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum CacheScope {
    /// Any client or shared cache may keep it: the result holds nothing
    /// user-specific. The default: the tool set is the same for everyone.
    #[default]
    Public,
    /// Only caches within the same authorization context may keep it.
    Private,
}

impl CacheScope {
    /// The wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }
}

/// Settings fixed when a connection starts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    pub(crate) page_size: usize,
    pub(crate) ttl_ms: u64,
    pub(crate) cache_scope: CacheScope,
    pub(crate) legacy: bool,
    pub(crate) server_info_meta: bool,
    pub(crate) list_changed: bool,
    pub(crate) workers: usize,
    pub(crate) max_queued_calls: usize,
    pub(crate) max_queued_bytes: usize,
    pub(crate) max_subscriptions: usize,
    pub(crate) drain_timeout: Duration,
    pub(crate) confirmation_timeout: Duration,
    pub(crate) max_confirmed_arguments: usize,
    pub(crate) limits: Limits,
    pub(crate) max_in_flight: usize,
}

impl Settings {
    fn new() -> Self {
        Self {
            page_size: 100,
            ttl_ms: 0,
            cache_scope: CacheScope::Public,
            legacy: true,
            server_info_meta: true,
            list_changed: true,
            workers: 4,
            max_queued_calls: 256,
            max_queued_bytes: 64 << 20,
            max_subscriptions: 16,
            drain_timeout: Duration::from_secs(10),
            confirmation_timeout: Duration::from_secs(600),
            max_confirmed_arguments: 4096,
            limits: Limits::mcp(),
            max_in_flight: 1024,
        }
    }
}

type AuditHook = Arc<dyn Fn(&AuditRecord<'_>) + Send + Sync>;

/// What every connection of one server shares.
pub(crate) struct Shared {
    pub(crate) info_json: String,
    pub(crate) instructions: Option<String>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) policy: Arc<dyn Policy>,
    pub(crate) audit: Option<AuditHook>,
    pub(crate) ledger: Arc<Ledger>,
    pub(crate) settings: Settings,
}

impl Shared {
    /// Closes a result object: `resultType` and `serverInfo` for the modern
    /// era; nothing more for the legacy one, whose results have neither.
    pub(crate) fn finish(&self, mut o: Obj, era: Era, result_type: &str) -> String {
        if era == Era::Modern {
            let _ = o.str("resultType", result_type);
            if self.settings.server_info_meta {
                let _ = o.with("_meta", |out| {
                    out.push_str("{\"");
                    out.push_str(crate::protocol::meta::SERVER_INFO);
                    out.push_str("\":");
                    out.push_str(&self.info_json);
                    out.push('}');
                });
            }
        }
        o.finish()
    }

    /// The `ServerCapabilities` object.
    pub(crate) fn capabilities_json(&self) -> String {
        if self.settings.list_changed {
            r#"{"tools":{"listChanged":true}}"#.to_owned()
        } else {
            r#"{"tools":{}}"#.to_owned()
        }
    }

    /// Passes `record` to the audit hook; a panicking hook is ignored.
    pub(crate) fn audit(&self, record: &AuditRecord<'_>) {
        if let Some(hook) = &self.audit {
            // The trail is best-effort: a broken hook must not take the
            // call (or the worker) down with it.
            let _ignored = catch_unwind(AssertUnwindSafe(|| hook(record)));
        }
    }
}

/// An MCP server: identity, tools, a permission policy, and settings.
///
/// The one-call path is [`Server::new`] → [`tool`](Self::tool) →
/// [`serve_stdio`](Self::serve_stdio). Each connection is served per MCP
/// 2026-07-28 (stateless, per-request `_meta`, `server/discover`) and, unless
/// [`with_legacy(false)`](Self::with_legacy), also accepts a 2025-11-25 or
/// 2025-06-18 client that opens with `initialize`.
///
/// # Examples
///
/// ```no_run
/// use mcp_io::{Implementation, Server, Tier, Tool, ToolResult, jsonx::json};
///
/// fn main() -> Result<(), Box<dyn std::error::Error>> {
///     Server::new(Implementation::new("adder", "1.0.0"))
///         .tool(
///             Tool::new("add", Tier::ReadOnly)
///                 .description("Adds two integers")
///                 .input_schema(json!({
///                     "type": "object",
///                     "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
///                     "required": ["a", "b"],
///                     "additionalProperties": false
///                 })),
///             |call| {
///                 let (a, b) = (call.arguments()["a"].as_i64(), call.arguments()["b"].as_i64());
///                 match a.zip(b).and_then(|(a, b)| a.checked_add(b)) {
///                     Some(sum) => Ok(ToolResult::text(sum.to_string())),
///                     None => Ok(ToolResult::error("the sum does not fit in 64 bits")),
///                 }
///             },
///         )?
///         .serve_stdio()?;
///     Ok(())
/// }
/// ```
pub struct Server {
    info: Implementation,
    instructions: Option<String>,
    registry: Arc<Registry>,
    policy: Arc<dyn Policy>,
    audit: Option<AuditHook>,
    ledger_capacity: usize,
    ledger: Arc<Ledger>,
    settings: Settings,
}

/// Connection numbers, for the registry's watchers.
static CONNECTIONS: AtomicU64 = AtomicU64::new(0);

impl Server {
    /// A server describing itself as `info`, with no tools, the default
    /// policy ([`Ceiling`]`(Tier::Mutating)`), and the default settings.
    ///
    /// # Examples
    ///
    /// ```
    /// let server = mcp_io::Server::new(mcp_io::Implementation::new("demo", "0.1.0"));
    /// assert!(server.tools().is_empty());
    /// ```
    #[must_use]
    pub fn new(info: Implementation) -> Self {
        Self {
            info,
            instructions: None,
            registry: Arc::new(Registry::new()),
            policy: Arc::new(Ceiling::default()),
            audit: None,
            ledger_capacity: 64,
            ledger: Arc::new(Ledger::new(64, Settings::new().confirmation_timeout)),
            settings: Settings::new(),
        }
    }

    /// Guidance for the model on how to use this server (MCP's
    /// `instructions`, in `server/discover` and `initialize` results).
    ///
    /// # Examples
    ///
    /// ```
    /// let server = mcp_io::Server::new(mcp_io::Implementation::new("demo", "0.1.0"))
    ///     .instructions("Prefer `search` before `open`.");
    /// # let _ = server;
    /// ```
    #[must_use]
    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.instructions = Some(text.into());
        self
    }

    /// Registers a tool. See [`Tools::add`].
    ///
    /// # Errors
    ///
    /// [`RegisterError`], as [`Tools::add`].
    ///
    /// # Examples
    ///
    /// See [`Server`].
    pub fn tool<F>(self, tool: Tool, handler: F) -> Result<Self, RegisterError>
    where
        F: Fn(&ToolCall<'_>) -> Result<ToolResult, ToolError> + Send + Sync + 'static,
    {
        self.registry.add(tool, Arc::new(handler))?;
        Ok(self)
    }

    /// Registers a typed tool. `A` is decoded from the call's arguments
    /// (after they pass the tool's input schema, which
    /// [`Tool::input_schema_for`](crate::Tool::input_schema_for) can take
    /// from `A`), and the handler's `R` is sent as structured content with a
    /// text copy, checked against the output schema if the tool has one.
    /// Arguments that fit the schema but not `A` are a tool execution error.
    ///
    /// `A` owns its data here. For arguments that borrow from the request
    /// (`&str`, `Cow<str>`) with no copy, register with [`tool`](Self::tool)
    /// and call [`ToolCall::decode`] in the handler.
    ///
    /// # Errors
    ///
    /// [`RegisterError`], as [`Tools::add`].
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Implementation, JsonSchema, Server, Tier, Tool, jsonx::{self, json, Value}};
    ///
    /// struct Add { a: i64, b: i64 }
    /// struct Sum { sum: i64 }
    ///
    /// impl<'de> jsonx::FromJson<'de> for Add {
    ///     fn from_json(reader: &mut jsonx::Reader<'de>) -> Result<Self, jsonx::DecodeError> {
    ///         let (mut a, mut b) = (None, None);
    ///         let mut object = reader.read_object()?;
    ///         while let Some(key) = object.next_key()? {
    ///             match &*key {
    ///                 "a" => object.field(&mut a)?,
    ///                 "b" => object.field(&mut b)?,
    ///                 _ => object.skip()?,
    ///             }
    ///         }
    ///         Ok(Add { a: object.required(a, "a")?, b: object.required(b, "b")? })
    ///     }
    /// }
    ///
    /// // The schema lives next to the decoder: one definition of the input.
    /// impl JsonSchema for Add {
    ///     fn json_schema() -> Value<'static> {
    ///         json!({
    ///             "type": "object",
    ///             "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
    ///             "required": ["a", "b"],
    ///             "additionalProperties": false
    ///         })
    ///     }
    /// }
    ///
    /// impl jsonx::ToJson for Sum {
    ///     fn to_json(&self, out: jsonx::ValueWriter<'_>) -> jsonx::Written {
    ///         out.object(|o| o.member("sum", &self.sum))
    ///     }
    /// }
    ///
    /// let server = Server::new(Implementation::new("adder", "1.0.0")).tool_json(
    ///     Tool::new("add", Tier::ReadOnly).input_schema_for::<Add>(),
    ///     |_, Add { a, b }| {
    ///         let sum = a.checked_add(b).ok_or_else(|| mcp_io::ToolError::new("the sum overflows 64 bits"))?;
    ///         Ok(Sum { sum })
    ///     },
    /// )?;
    /// let (tx, rx) = std::sync::mpsc::channel::<String>();
    /// let mut connection = server.connect(tx)?; // kept open while the call runs
    /// connection.handle(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{
    ///     "io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},
    ///     "name":"add","arguments":{"a":2,"b":40}}}"#)?;
    /// assert!(server.tools().contains("add"));
    /// assert!(rx.recv_timeout(std::time::Duration::from_secs(10))?.contains(r#""structuredContent":{"sum":42}"#));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn tool_json<A, R, F>(self, tool: Tool, handler: F) -> Result<Self, RegisterError>
    where
        A: for<'de> crate::jsonx::FromJson<'de>,
        R: crate::jsonx::ToJson,
        F: Fn(&ToolCall<'_>, A) -> Result<R, ToolError> + Send + Sync + 'static,
    {
        self.registry.add(tool, crate::registry::typed(handler))?;
        Ok(self)
    }

    /// The tool set, to change while serving. Connections see changes at
    /// once.
    ///
    /// # Examples
    ///
    /// See [`Tools`].
    #[must_use]
    pub fn tools(&self) -> Tools {
        Tools(Arc::clone(&self.registry))
    }

    /// Sets the permission policy (see [`permission`](crate::permission)).
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Decision, Implementation, Server, Tier};
    ///
    /// let server = Server::new(Implementation::new("demo", "0.1.0")).policy(|request: &mcp_io::PermissionRequest<'_>| {
    ///     if request.tier() >= Tier::Destructive { Decision::deny("read-only mode") } else { Decision::Allow }
    /// });
    /// # let _ = server;
    /// ```
    #[must_use]
    pub fn policy(mut self, policy: impl Policy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    /// Calls `hook` once per `tools/call` request, refused ones included,
    /// with its [`AuditRecord`] (tool,
    /// tier, request id, client name, outcome, time taken; never arguments
    /// or results). The hook runs on the worker that handled the call (on
    /// the reading thread for a call refused because the queue was full); a
    /// panicking hook is ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// let server = mcp_io::Server::new(mcp_io::Implementation::new("demo", "0.1.0"))
    ///     .on_audit(|record| eprintln!("{:?} [{:?}] {}", record.tool(), record.tier(), record.outcome()));
    /// # let _ = server;
    /// ```
    #[must_use]
    pub fn on_audit(mut self, hook: impl Fn(&AuditRecord<'_>) + Send + Sync + 'static) -> Self {
        self.audit = Some(Arc::new(hook));
        self
    }

    /// Tools per `tools/list` page (at least 1; default 100).
    #[must_use]
    pub fn with_page_size(mut self, size: usize) -> Self {
        self.settings.page_size = size.max(1);
        self
    }

    /// The caching hints on `tools/list` and `server/discover` results:
    /// `ttlMs` (default 0, "immediately stale") and `cacheScope` (default
    /// public).
    #[must_use]
    pub fn with_cache(mut self, ttl: Duration, scope: CacheScope) -> Self {
        self.settings.ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        self.settings.cache_scope = scope;
        self
    }

    /// Whether connections also serve legacy clients that open with
    /// `initialize` (2025-11-25, 2025-06-18). Default `true`. Without it, an
    /// `initialize` is answered with `UnsupportedProtocolVersion` naming the
    /// supported versions, as MCP asks of a modern-only server.
    #[must_use]
    pub fn with_legacy(mut self, legacy: bool) -> Self {
        self.settings.legacy = legacy;
        self
    }

    /// Whether 2026-07-28 results carry `io.modelcontextprotocol/serverInfo`
    /// in `_meta` (MCP SHOULD, unless configured not to). Default `true`.
    #[must_use]
    pub fn with_server_info_meta(mut self, include: bool) -> Self {
        self.settings.server_info_meta = include;
        self
    }

    /// Whether the `tools` capability advertises `listChanged` and
    /// `subscriptions/listen` honours `toolsListChanged`. Default `true`.
    #[must_use]
    pub fn with_list_changed(mut self, list_changed: bool) -> Self {
        self.settings.list_changed = list_changed;
        self
    }

    /// Worker threads per connection for tool calls (default 4). `0` runs
    /// calls inline on the reading thread: deterministic, but a running call
    /// then cannot be cancelled, and a legacy confirmation (which waits for
    /// the client's answer) cannot be obtained.
    #[must_use]
    pub fn with_workers(mut self, workers: usize) -> Self {
        self.settings.workers = workers;
        self
    }

    /// Tool calls per connection that may run or wait for a worker at once
    /// (default 256). Past it, a call is refused with `-32603` ("the server
    /// is busy").
    #[must_use]
    pub fn with_max_queued_calls(mut self, calls: usize) -> Self {
        self.settings.max_queued_calls = calls.max(1);
        self
    }

    /// The params, in bytes, that tool calls running or waiting on one
    /// connection may hold between them (default 64 MiB). Past it, a call is
    /// refused with `-32603` ("the server is busy"); a call larger than the
    /// limit is still taken when no other is in flight.
    #[must_use]
    pub fn with_max_queued_bytes(mut self, bytes: usize) -> Self {
        self.settings.max_queued_bytes = bytes;
        self
    }

    /// Open `subscriptions/listen` streams per connection (default 16).
    #[must_use]
    pub fn with_max_subscriptions(mut self, streams: usize) -> Self {
        self.settings.max_subscriptions = streams;
        self
    }

    /// How long serving waits, once the input ends, for running tool calls
    /// to answer before closing (default 10 s). Calls still running then are
    /// cancelled.
    #[must_use]
    pub fn with_drain_timeout(mut self, timeout: Duration) -> Self {
        self.settings.drain_timeout = timeout;
        self
    }

    /// How long a confirmation stays valid (default 10 minutes): the life of
    /// a 2026-07-28 `requestState`, and how long a legacy session's
    /// `elicitation/create` is waited for.
    #[must_use]
    pub fn with_confirmation_timeout(mut self, timeout: Duration) -> Self {
        self.settings.confirmation_timeout = timeout;
        self.ledger = Arc::new(Ledger::new(self.ledger_capacity, timeout));
        self
    }

    /// The largest arguments, in bytes as shown to the user, a call that
    /// needs confirmation may have (default 4 KiB). The user always sees a
    /// call's arguments in full (canonical JSON, non-ASCII escaped); a call
    /// whose arguments would not fit is refused with a tool error rather
    /// than shown in part.
    #[must_use]
    pub fn with_max_confirmed_arguments(mut self, bytes: usize) -> Self {
        self.settings.max_confirmed_arguments = bytes;
        self
    }

    /// Outstanding confirmations each connection may have (default 64);
    /// past it, that connection's oldest is forgotten. Confirmations are
    /// kept server-wide (a retry may arrive on another connection), each as
    /// a digest of its arguments.
    #[must_use]
    pub fn with_max_pending_confirmations(mut self, count: usize) -> Self {
        self.ledger_capacity = count.max(1);
        self.ledger = Arc::new(Ledger::new(
            self.ledger_capacity,
            self.settings.confirmation_timeout,
        ));
        self
    }

    /// Budgets on incoming messages (rpc-io's [`Limits`]; default
    /// [`Limits::mcp`]). Batches stay refused whatever `max_batch` says:
    /// MCP has had none since 2025-06-18.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.settings.limits = limits.without_batches();
        self
    }

    /// Budgets for compiling and applying tool schemas registered from now
    /// on (default [`SchemaLimits::new`]).
    #[must_use]
    pub fn with_schema_limits(self, limits: SchemaLimits) -> Self {
        self.registry.set_limits(limits);
        self
    }

    fn shared(&self) -> Arc<Shared> {
        let ledger = Arc::clone(&self.ledger);
        Arc::new(Shared {
            info_json: self.info.to_json(),
            instructions: self.instructions.clone(),
            registry: Arc::clone(&self.registry),
            policy: Arc::clone(&self.policy),
            audit: self.audit.clone(),
            ledger,
            settings: self.settings,
        })
    }

    /// A connection that sends through `sink`: the building block for
    /// transports other than [`serve`](Self::serve) (an in-process test
    /// client, a socket, a pipe). Feed it with [`Connection::handle`] or
    /// [`Connection::run`].
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if a worker thread cannot be started.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Implementation, Server};
    ///
    /// let server = Server::new(Implementation::new("demo", "0.1.0"));
    /// let (tx, rx) = std::sync::mpsc::channel::<String>();
    /// let mut connection = server.connect(tx)?;
    /// connection.handle(br#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{
    ///     "io.modelcontextprotocol/protocolVersion":"2026-07-28",
    ///     "io.modelcontextprotocol/clientCapabilities":{}}}}"#)?;
    /// assert!(rx.try_recv().is_ok_and(|reply| reply.contains(r#""supportedVersions":["2026-07-28"]"#)));
    /// # Ok::<(), mcp_io::Error>(())
    /// ```
    pub fn connect<K: Sink>(&self, sink: K) -> Result<Connection<K>, Error> {
        let shared = self.shared();
        let settings = shared.settings;
        let id = CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        let session = Session::new(shared, id)?;
        // rpc-io's MCP preset is the 2026-07-28 one: no gate (the legacy
        // handshake is this crate's, per connection), batches off, MCP's
        // cancellation (no reply), unreadable messages answered without an
        // `id`, and `-32603` for overload, so nothing in the
        // `-32000..=-32019` range or `-32002` is ever sent.
        let config = Config::mcp()
            .with_limits(settings.limits)
            .with_gate(None)
            .with_max_in_flight(settings.max_in_flight);
        let endpoint = Endpoint::new(sink, session::router(&settings), session).with_config(config);
        let peer = endpoint.peer();
        let mut connection = Connection { endpoint };
        connection.endpoint.state_mut().set_peer(peer);
        Ok(connection)
    }

    /// Serves one connection over a byte stream, newline-framed as MCP's
    /// stdio transport: reads `input` until it ends, answering through
    /// `output`, then waits up to the drain timeout for running tool calls
    /// and returns.
    ///
    /// Output goes through rpc-io's bounded queue and writer thread, so a
    /// client that stops reading cannot stall the server (past the queue's
    /// limits the connection closes).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if a thread cannot start; [`Error::Rpc`] if reading or
    /// writing fails, or the input breaks the framing beyond recovery.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Implementation, Server};
    ///
    /// let input: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"c\",\"version\":\"1\"}}}\n";
    /// let server = Server::new(Implementation::new("demo", "0.1.0"));
    /// server.serve(input, std::io::sink())?;
    /// # Ok::<(), mcp_io::Error>(())
    /// ```
    pub fn serve<R, W>(&self, input: R, output: W) -> Result<End, Error>
    where
        R: Read,
        W: Write + Send + 'static,
    {
        let sink = Queued::new(output, Framing::Newline)?;
        let mut connection = self.connect(sink)?;
        let end = connection.run(input);
        connection.close();
        end
    }

    /// Serves one connection on this process's standard input and output
    /// (MCP's stdio transport) until standard input ends.
    ///
    /// Only MCP messages are written to standard output; this crate writes
    /// nothing else anywhere (log to standard error yourself, and never to
    /// standard output).
    ///
    /// # Errors
    ///
    /// As [`serve`](Self::serve).
    ///
    /// # Examples
    ///
    /// See [`Server`].
    pub fn serve_stdio(&self) -> Result<End, Error> {
        self.serve(std::io::stdin().lock(), std::io::stdout())
    }
}

impl core::fmt::Debug for Server {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Server")
            .field("info", &self.info)
            .field("tools", &self.registry.names())
            .finish_non_exhaustive()
    }
}

/// One served connection.
///
/// Built by [`Server::connect`] over any rpc-io [`Sink`]; fed whole messages
/// with [`handle`](Self::handle), or a newline-framed byte stream with
/// [`run`](Self::run). Dropping it closes it.
pub struct Connection<K: Sink> {
    endpoint: Endpoint<Session, K>,
}

impl<K: Sink> Connection<K> {
    /// Handles one message (a single line's JSON, without the newline).
    /// Malformed input is answered, not returned as an error. Tool calls run
    /// on the worker threads and answer through the sink when they finish.
    ///
    /// # Errors
    ///
    /// [`Error::Rpc`] if the connection is closed or its sink failed.
    ///
    /// # Examples
    ///
    /// See [`Server::connect`].
    pub fn handle(&mut self, message: &[u8]) -> Result<Flow, Error> {
        Ok(self.endpoint.handle(message)?)
    }

    /// Reads newline-framed messages from `input` and handles them until it
    /// ends; then waits up to the drain timeout for running tool calls, and
    /// closes. A last line without its newline is handled too.
    ///
    /// # Errors
    ///
    /// [`Error::Rpc`]: a read failed, the sink failed, or the input broke
    /// the framing beyond recovery. The connection is closed after any of
    /// them.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Implementation, Server};
    ///
    /// let server = Server::new(Implementation::new("demo", "0.1.0"));
    /// let (tx, rx) = std::sync::mpsc::channel::<String>();
    /// let mut connection = server.connect(tx)?;
    /// // A legacy ping, without the newline at the end of the stream.
    /// connection.run(&br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#[..])?;
    /// assert_eq!(rx.try_recv().ok().as_deref(), Some(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#));
    /// assert!(connection.is_closed());
    /// # Ok::<(), mcp_io::Error>(())
    /// ```
    pub fn run<R: Read>(&mut self, mut input: R) -> Result<End, Error> {
        let drain = self.endpoint.state().drain_timeout();
        loop {
            let buf = match self.endpoint.next_read(Framing::Newline)? {
                Next::Read(buf) => buf,
                Next::Stopped => return Ok(End::Stopped),
            };
            let n = match input.read(buf) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.endpoint.close();
                    return Err(Error::Rpc(rpc_io::Error::Io(e)));
                }
            };
            if n == 0 {
                // End of input. A last line without its newline is still a
                // message: end it, handle it, let running calls answer, then
                // close.
                if let Next::Read(buf) = self.endpoint.next_read(Framing::Newline)? {
                    if let Some(first) = buf.first_mut() {
                        *first = b'\n';
                        self.endpoint.commit(1);
                    }
                }
                if let Next::Stopped = self.endpoint.next_read(Framing::Newline)? {
                    return Ok(End::Stopped);
                }
                let _drained = self.endpoint.state().wait_idle(drain);
                return Ok(self.endpoint.finish()?);
            }
            self.endpoint.commit(n);
        }
    }

    /// Waits until no tool call is running or queued, for at most `timeout`.
    /// Whether that happened.
    ///
    /// # Examples
    ///
    /// ```
    /// let server = mcp_io::Server::new(mcp_io::Implementation::new("demo", "0.1.0"));
    /// let connection = server.connect(std::sync::mpsc::channel::<String>().0)?;
    /// assert!(connection.wait_idle(std::time::Duration::ZERO));
    /// # Ok::<(), mcp_io::Error>(())
    /// ```
    #[must_use]
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        self.endpoint.state().wait_idle(timeout)
    }

    /// Closes the connection from the server's side: each open
    /// `subscriptions/listen` stream first gets its graceful-closure
    /// response (MCP: the server SHOULD answer the listen request when it
    /// ends a subscription) and then `notifications/cancelled` naming it
    /// (MCP: the server MUST send one for a stream it tears down); then
    /// running tool calls are cancelled (their answers are dropped).
    /// Idempotent.
    ///
    /// When the client ends the session (closing its output, which ends
    /// [`run`](Self::run)), no closure responses are sent: no one is
    /// listening.
    pub fn close(&mut self) {
        if !self.endpoint.is_closed() {
            self.endpoint.state_mut().end_subscriptions();
        }
        self.endpoint.close();
    }

    /// Whether the connection has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.endpoint.is_closed()
    }

    /// The legacy session's state (for a connection that used `initialize`).
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Implementation, Lifecycle, Server};
    ///
    /// let server = Server::new(Implementation::new("demo", "0.1.0"));
    /// let connection = server.connect(std::sync::mpsc::channel::<String>().0)?;
    /// assert_eq!(connection.lifecycle(), &Lifecycle::Stateless);
    /// # Ok::<(), mcp_io::Error>(())
    /// ```
    #[must_use]
    pub fn lifecycle(&self) -> &Lifecycle {
        self.endpoint.state().lifecycle()
    }

    /// A handle to send through this connection from any thread.
    #[must_use]
    pub fn peer(&self) -> Peer {
        self.endpoint.peer()
    }

    /// The connection's message counters (rpc-io's [`Stats`]).
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.endpoint.stats()
    }
}
