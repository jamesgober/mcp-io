//! One connection's state and the methods it serves.
//!
//! The modern path keeps no state between requests: each one is classified
//! by its own `_meta` and answered from it. The legacy path keeps exactly
//! what a 2025-11-25 session needs: the negotiated version and the client's
//! declared capabilities, set by `initialize`.

use std::{sync::Arc, time::Duration};

use crate::{
    call::{self, Job},
    json::{self, Obj},
    jsonx::Value,
    pool::Pool,
    protocol::{self, Classified, Era, LEGACY_PROTOCOL_VERSIONS, MODERN_PROTOCOL_VERSIONS},
    rpc_io::{ErrorObject, Params, Peer, Raw, Responder, Router},
    server::{Settings, Shared},
};

/// Where a connection's legacy (`initialize`-based) session stands.
///
/// Modern (2026-07-28) requests never read or change it: a connection that
/// only serves modern requests stays [`Stateless`](Self::Stateless).
///
/// ```text
///   Stateless --initialize--> Initializing --notifications/initialized--> Operating
/// ```
///
/// # Examples
///
/// ```
/// use mcp_io::{Implementation, Lifecycle, Server};
///
/// let server = Server::new(Implementation::new("demo", "0.1.0"));
/// let (tx, _rx) = std::sync::mpsc::channel::<String>();
/// let mut connection = server.connect(tx)?;
/// connection.handle(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
///     "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}"#)?;
/// assert_eq!(connection.lifecycle(), &Lifecycle::Initializing { protocol_version: "2025-11-25" });
/// connection.handle(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)?;
/// assert_eq!(connection.lifecycle(), &Lifecycle::Operating { protocol_version: "2025-11-25" });
/// # Ok::<(), mcp_io::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Lifecycle {
    /// No `initialize` has been answered. Modern requests are served;
    /// requests without `_meta` (other than `initialize` and `ping`) are
    /// refused with `-32602`.
    Stateless,
    /// `initialize` was answered with `protocol_version`; waiting for
    /// `notifications/initialized`. Requests are served already (a client
    /// may send them once it has the response).
    Initializing {
        /// The negotiated revision.
        protocol_version: &'static str,
    },
    /// The client confirmed with `notifications/initialized`; list changes
    /// are notified from now on.
    Operating {
        /// The negotiated revision.
        protocol_version: &'static str,
    },
}

impl Lifecycle {
    /// The negotiated legacy revision, once `initialize` was answered.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::Lifecycle::Stateless.protocol_version(), None);
    /// ```
    #[must_use]
    pub fn protocol_version(&self) -> Option<&'static str> {
        match self {
            Self::Stateless => None,
            Self::Initializing { protocol_version } | Self::Operating { protocol_version } => {
                Some(protocol_version)
            }
        }
    }
}

/// The era a request is served under, with what it needs from each.
pub(crate) enum Served<'v> {
    Modern(protocol::Modern<'v>),
    Legacy(&'static str),
}

/// A refused request: its responder and the error to answer with. Boxed:
/// the pair is large, and only the refusal path pays for the allocation.
type Refusal = Box<(Responder, ErrorObject)>;

fn refuse(responder: Responder, error: ErrorObject) -> Refusal {
    Box::new((responder, error))
}

pub(crate) struct Session {
    shared: Arc<Shared>,
    connection: u64,
    lifecycle: Lifecycle,
    legacy_capabilities: Value<'static>,
    legacy_client_info: Option<Value<'static>>,
    subscriptions: Vec<Responder>,
    pool: Pool,
    peer: Option<Peer>,
}

impl Session {
    pub(crate) fn new(shared: Arc<Shared>, connection: u64) -> std::io::Result<Self> {
        let settings = &shared.settings;
        let pool = Pool::new(
            settings.workers,
            settings.max_queued_calls,
            settings.max_queued_bytes,
        )?;
        Ok(Self {
            shared,
            connection,
            lifecycle: Lifecycle::Stateless,
            legacy_capabilities: Value::Object(crate::jsonx::Object::new()),
            legacy_client_info: None,
            subscriptions: Vec::new(),
            pool,
            peer: None,
        })
    }

    pub(crate) fn set_peer(&mut self, peer: Peer) {
        self.peer = Some(peer);
    }

    pub(crate) fn lifecycle(&self) -> &Lifecycle {
        &self.lifecycle
    }

    pub(crate) fn drain_timeout(&self) -> Duration {
        self.shared.settings.drain_timeout
    }

    pub(crate) fn wait_idle(&self, timeout: Duration) -> bool {
        self.pool.wait_idle(timeout)
    }

    /// Classifies a request: modern by its `_meta`, else legacy if a session
    /// was opened.
    fn era<'v>(&self, params: &'v Value<'v>) -> Result<Served<'v>, ErrorObject> {
        match protocol::classify(params)? {
            Classified::Modern(modern) => Ok(Served::Modern(modern)),
            Classified::Unversioned => match self.lifecycle.protocol_version() {
                Some(version) => Ok(Served::Legacy(version)),
                None if self.shared.settings.legacy => Err(ErrorObject::invalid_params(
                    "missing required `_meta.io.modelcontextprotocol/protocolVersion` \
                     (clients of 2025-11-25 and earlier must send `initialize` first)",
                )),
                None => Err(ErrorObject::invalid_params(
                    "missing required `_meta.io.modelcontextprotocol/protocolVersion`",
                )),
            },
        }
    }

    fn initialize(&mut self, params: &Params<'_>) -> Result<Raw<'static>, ErrorObject> {
        let value = params.parse()?;
        let requested = value.get("protocolVersion").and_then(Value::as_str);
        if !self.shared.settings.legacy {
            return Err(protocol::unsupported_version(requested.unwrap_or_default()));
        }
        if self.lifecycle != Lifecycle::Stateless {
            return Err(ErrorObject::invalid_request(
                "the session is already initialized",
            ));
        }
        let requested = requested.ok_or_else(|| {
            ErrorObject::invalid_params("`initialize` needs `protocolVersion` (a string)")
        })?;
        let capabilities = value
            .get("capabilities")
            .filter(|c| c.as_object().is_some())
            .ok_or_else(|| {
                ErrorObject::invalid_params("`initialize` needs `capabilities` (an object)")
            })?;
        let client_info = value
            .get("clientInfo")
            .filter(|c| c.as_object().is_some())
            .ok_or_else(|| {
                ErrorObject::invalid_params("`initialize` needs `clientInfo` (an object)")
            })?;
        // The requested version if served, else the newest legacy one: the
        // client decides whether it can live with that.
        let negotiated = LEGACY_PROTOCOL_VERSIONS
            .iter()
            .copied()
            .find(|v| *v == requested)
            .unwrap_or(LEGACY_PROTOCOL_VERSIONS[0]);
        self.legacy_capabilities = capabilities.clone().into_owned();
        self.legacy_client_info = Some(client_info.clone().into_owned());
        self.lifecycle = Lifecycle::Initializing {
            protocol_version: negotiated,
        };
        let capabilities = self.shared.capabilities_json();
        let mut o = Obj::with_capacity(256);
        let _ = o
            .str("protocolVersion", negotiated)
            .raw("capabilities", &capabilities)
            .raw("serverInfo", &self.shared.info_json)
            .opt_str("instructions", self.shared.instructions.as_deref());
        json::raw(self.shared.finish(o, Era::Legacy, "complete"))
    }

    fn initialized(&mut self) {
        if let Lifecycle::Initializing { protocol_version } = self.lifecycle {
            self.lifecycle = Lifecycle::Operating { protocol_version };
            if self.shared.settings.list_changed {
                if let Some(peer) = &self.peer {
                    self.shared
                        .registry
                        .watch(self.connection, peer.clone(), None);
                }
            }
        }
        // Otherwise (before `initialize`, or a repeat) it changes nothing.
    }

    fn ping(&self, params: &Params<'_>) -> Result<Raw<'static>, ErrorObject> {
        let value = params.parse()?;
        // `ping` is legacy only (2026-07-28 removed it), and allowed before
        // `initialize`.
        match protocol::classify(&value)? {
            Classified::Unversioned if self.shared.settings.legacy => {
                Ok(Raw::from_string("{}".to_owned())
                    .map_err(|_| ErrorObject::internal_error("unreachable"))?)
            }
            _ => Err(ErrorObject::method_not_found("ping")),
        }
    }

    fn discover(&self, params: &Params<'_>) -> Result<Raw<'static>, ErrorObject> {
        let value = params.parse()?;
        match self.era(&value)? {
            Served::Modern(_) => {}
            Served::Legacy(_) => return Err(ErrorObject::method_not_found("server/discover")),
        }
        let settings = &self.shared.settings;
        let capabilities = self.shared.capabilities_json();
        let mut o = Obj::with_capacity(256);
        let _ = o
            .with("supportedVersions", |out| {
                out.push('[');
                for (i, version) in MODERN_PROTOCOL_VERSIONS.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    crate::jsonx::write_string(out, version);
                }
                out.push(']');
            })
            .raw("capabilities", &capabilities)
            .opt_str("instructions", self.shared.instructions.as_deref())
            .u64("ttlMs", settings.ttl_ms)
            .str("cacheScope", settings.cache_scope.as_str());
        json::raw(self.shared.finish(o, Era::Modern, "complete"))
    }

    fn list_tools(&self, params: &Params<'_>) -> Result<Raw<'static>, ErrorObject> {
        let value = params.parse()?;
        let era = match self.era(&value)? {
            Served::Modern(_) => Era::Modern,
            Served::Legacy(_) => Era::Legacy,
        };
        let cursor = match value.get("cursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(cursor)) => Some(cursor.as_ref()),
            Some(_) => return Err(ErrorObject::invalid_params("`cursor` must be a string")),
        };
        let settings = &self.shared.settings;
        let (tools, next) = self
            .shared
            .registry
            .page(cursor, settings.page_size)
            .ok_or_else(|| {
                ErrorObject::invalid_params("invalid cursor: list from the start, without one")
            })?;
        let mut o = Obj::with_capacity(tools.len() + 128);
        let _ = o.with("tools", |out| {
            out.push('[');
            out.push_str(&tools);
            out.push(']');
        });
        let _ = o.opt_str("nextCursor", next.as_deref());
        if era == Era::Modern {
            let _ = o
                .u64("ttlMs", settings.ttl_ms)
                .str("cacheScope", settings.cache_scope.as_str());
        }
        json::raw(self.shared.finish(o, era, "complete"))
    }

    fn call_tool(&mut self, params: &Params<'_>, responder: Responder) {
        match self.prepare_call(params, responder) {
            Ok(()) => {}
            Err(refused) => {
                let (responder, error) = *refused;
                // Cancelled or closed already: no one is waiting for it.
                let _unanswerable = responder.err(error);
            }
        }
    }

    fn prepare_call(&mut self, params: &Params<'_>, responder: Responder) -> Result<(), Refusal> {
        let value = match params.parse() {
            Ok(value) => value,
            Err(error) => return Err(refuse(responder, error)),
        };
        let (era, version, capabilities, client_info) = match self.era(&value) {
            Ok(Served::Modern(m)) => (
                Era::Modern,
                m.version,
                m.capabilities.clone().into_owned(),
                m.client_info.map(|info| info.clone().into_owned()),
            ),
            Ok(Served::Legacy(version)) => (
                Era::Legacy,
                version,
                self.legacy_capabilities.clone(),
                self.legacy_client_info.clone(),
            ),
            Err(error) => return Err(refuse(responder, error)),
        };
        let Some(name) = value.get("name").and_then(Value::as_str) else {
            return Err(refuse(
                responder,
                ErrorObject::invalid_params("`tools/call` needs `name` (a string)"),
            ));
        };
        match value.get("arguments") {
            None | Some(Value::Null | Value::Object(_)) => {}
            Some(_) => {
                return Err(refuse(
                    responder,
                    ErrorObject::invalid_params("`arguments` must be an object"),
                ));
            }
        }
        let Some(entry) = self.shared.registry.get(name) else {
            let message = format!("Unknown tool: {}", protocol::clip(name, 128));
            return Err(refuse(responder, ErrorObject::invalid_params(message)));
        };
        // What the job will hold: the parsed params, about their text's size.
        let bytes = params.raw().map_or(0, |raw| raw.as_str().len());
        if !self.pool.has_room(bytes) {
            return Err(refuse(
                responder,
                ErrorObject::internal_error("the server is busy; retry the call later"),
            ));
        }
        let progress = responder.progress(params);
        let job = Job {
            shared: Arc::clone(&self.shared),
            entry,
            params: value.into_owned(),
            era,
            version,
            capabilities,
            client_info,
            progress,
            responder,
            received: std::time::Instant::now(),
            connection: self.connection,
        };
        // Only this thread submits, and the load was checked above, so the
        // pool has room; were it full anyway, the job (and its responder) is
        // dropped, which answers with an internal error.
        let _submitted = self.pool.submit(bytes, move || call::run(job));
        Ok(())
    }

    fn listen(&mut self, params: &Params<'_>, responder: Responder) {
        if let Err(refused) = self.open_subscription(params, responder) {
            let (responder, error) = *refused;
            let _unanswerable = responder.err(error);
        }
    }

    fn open_subscription(
        &mut self,
        params: &Params<'_>,
        responder: Responder,
    ) -> Result<(), Refusal> {
        let value = match params.parse() {
            Ok(value) => value,
            Err(error) => return Err(refuse(responder, error)),
        };
        match self.era(&value) {
            Ok(Served::Modern(_)) => {}
            Ok(Served::Legacy(_)) => {
                return Err(refuse(
                    responder,
                    ErrorObject::method_not_found("subscriptions/listen"),
                ));
            }
            Err(error) => return Err(refuse(responder, error)),
        }
        let Some(filter) = value.get("notifications").and_then(Value::as_object) else {
            return Err(refuse(
                responder,
                ErrorObject::invalid_params(
                    "`subscriptions/listen` needs `notifications` (an object)",
                ),
            ));
        };
        for flag in [
            "toolsListChanged",
            "promptsListChanged",
            "resourcesListChanged",
        ] {
            if filter.get(flag).is_some_and(|v| v.as_bool().is_none()) {
                return Err(refuse(
                    responder,
                    ErrorObject::invalid_params(format!(
                        "`notifications.{flag}` must be a boolean"
                    )),
                ));
            }
        }
        if let Some(uris) = filter.get("resourceSubscriptions") {
            let valid = uris
                .as_array()
                .is_some_and(|a| a.iter().all(|u| u.as_str().is_some()));
            if !valid {
                return Err(refuse(
                    responder,
                    ErrorObject::invalid_params(
                        "`notifications.resourceSubscriptions` must be an array of strings",
                    ),
                ));
            }
        }
        self.subscriptions.retain(|open| !open.is_cancelled());
        let limit = self.shared.settings.max_subscriptions;
        if self.subscriptions.len() >= limit {
            let message =
                format!("too many open subscriptions on this connection (the limit is {limit})");
            return Err(refuse(responder, ErrorObject::invalid_request(message)));
        }
        let Some(peer) = self.peer.clone() else {
            return Err(refuse(
                responder,
                ErrorObject::internal_error("the connection is not ready"),
            ));
        };
        let tools = self.shared.settings.list_changed
            && filter.get("toolsListChanged").is_some_and(|v| v == &true);
        let id = responder.id().to_string();
        let mut ack = Obj::new();
        let _ = ack
            .with("_meta", |out| {
                out.push_str("{\"");
                out.push_str(protocol::meta::SUBSCRIPTION_ID);
                out.push_str("\":");
                out.push_str(&id);
                out.push('}');
            })
            .raw(
                "notifications",
                if tools {
                    r#"{"toolsListChanged":true}"#
                } else {
                    "{}"
                },
            );
        let ack = match json::raw(ack.finish()) {
            Ok(ack) => ack,
            Err(error) => return Err(refuse(responder, error)),
        };
        // The acknowledgment goes out before the watcher exists, so no list
        // change can overtake it on this subscription.
        if peer
            .notify("notifications/subscriptions/acknowledged", ack)
            .is_err()
        {
            // The connection is closing; the stream ends with it.
            return Ok(());
        }
        if tools {
            self.shared
                .registry
                .watch(self.connection, peer, Some((id, responder.token())));
        }
        // The stream stays open: the request is answered only when the
        // client cancels it (no response then) or the connection ends.
        self.subscriptions.push(responder);
        Ok(())
    }
}

impl Session {
    /// Answers every open subscription with its graceful-closure result.
    pub(crate) fn end_subscriptions(&mut self) {
        // No list change may follow a stream's closure.
        self.shared.registry.unwatch(self.connection);
        for responder in self.subscriptions.drain(..) {
            if responder.is_cancelled() {
                continue;
            }
            let id = responder.id().to_string();
            let mut o = Obj::new();
            let _ = o.str("resultType", "complete").with("_meta", |out| {
                out.push_str("{\"");
                out.push_str(protocol::meta::SUBSCRIPTION_ID);
                out.push_str("\":");
                out.push_str(&id);
                if self.shared.settings.server_info_meta {
                    out.push_str(",\"");
                    out.push_str(protocol::meta::SERVER_INFO);
                    out.push_str("\":");
                    out.push_str(&self.shared.info_json);
                }
                out.push('}');
            });
            let peer = responder.peer();
            let answered = json::raw(o.finish()).map(|raw| responder.ok(raw));
            // The spec asks for both: the listen request's response signals
            // a graceful end (Subscriptions), and the server "MUST send
            // `notifications/cancelled`" for a stream it tears down
            // (Cancellation). The response goes first, so a client that
            // stops at the cancellation has already seen it.
            if matches!(answered, Ok(Ok(()))) {
                let mut params = Obj::new();
                let _ = params
                    .raw("requestId", &id)
                    .str("reason", "the server closed the subscription");
                let sent = json::raw(params.finish())
                    .map(|raw| peer.notify("notifications/cancelled", raw));
                // The connection closes next; a failed send changes nothing.
                let _closing = sent;
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.registry.unwatch(self.connection);
    }
}

/// The methods every connection serves.
pub(crate) fn router(_settings: &Settings) -> Router<Session> {
    Router::new()
        .request("initialize", |s: &mut Session, params, _| {
            s.initialize(&params)
        })
        .notification("notifications/initialized", |s: &mut Session, _, _| {
            s.initialized()
        })
        .request("ping", |s: &mut Session, params, _| s.ping(&params))
        .request("server/discover", |s: &mut Session, params, _| {
            s.discover(&params)
        })
        .request("tools/list", |s: &mut Session, params, _| {
            s.list_tools(&params)
        })
        .deferred("tools/call", |s: &mut Session, params, _, responder| {
            s.call_tool(&params, responder)
        })
        .deferred(
            "subscriptions/listen",
            |s: &mut Session, params, _, responder| s.listen(&params, responder),
        )
}
