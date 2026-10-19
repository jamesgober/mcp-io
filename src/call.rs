//! Running one `tools/call`: validation, the permission gate, confirmation,
//! the handler, and the answer.
//!
//! Runs on a worker thread (see [`pool`](crate::pool)). Every step that can
//! stop the call answers it exactly once: a result (`isError` for a tool
//! execution error), a JSON-RPC error, an `InputRequiredResult`, or nothing
//! when the client cancelled it.

use std::{
    future::Future as _,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Instant,
};

use crate::{
    AuditRecord, Decision, Error, Outcome, PermissionRequest, Tier, ToolError, ToolResult,
    content::ToolErrorKind,
    json::{self, Obj},
    jsonx::{self, Value},
    permission::{self, Redeem},
    protocol::{self, Era},
    registry::Entry,
    rpc_io::{CallError, CancelToken, ErrorObject, Id, Progress, Raw, Responder},
    server::Shared,
};

/// A validation report sent to the client is clipped to this many bytes.
/// (Each error is short already: property names in it are clipped to 64
/// bytes and paths to 512.)
const REPORT_CLIP: usize = 8192;

/// The `params` of a call that sent none.
const NO_PARAMS: &str = "{}";

/// The key of the confirmation in `inputRequests` / `inputResponses`.
const CONFIRM_KEY: &str = "confirm";

/// One call, moved to a worker: its text, unparsed, and what the connection
/// knows that the request does not say.
pub(crate) struct Job {
    pub(crate) shared: Arc<Shared>,
    /// The request's `params` text (`None` when absent). Parsed once, on the
    /// worker; the tree and typed arguments borrow from it.
    pub(crate) text: Option<Raw<'static>>,
    /// The connection's legacy session, if `initialize` opened one.
    pub(crate) legacy: Option<LegacySession>,
    pub(crate) progress: Option<Progress>,
    pub(crate) responder: Responder,
    pub(crate) received: Instant,
    /// The connection the call came in on (for the confirmation quota).
    pub(crate) connection: u64,
}

/// What a legacy session negotiated, copied into the calls it makes.
pub(crate) struct LegacySession {
    pub(crate) version: &'static str,
    pub(crate) capabilities: Value<'static>,
    pub(crate) client_info: Option<Value<'static>>,
}

/// A call that passed the request checks: its tool and who is asking.
struct Prepared<'j> {
    shared: &'j Arc<Shared>,
    entry: Arc<Entry>,
    text: Option<&'j Raw<'static>>,
    era: Era,
    version: &'static str,
    capabilities: Value<'static>,
    client_info: Option<Value<'static>>,
    progress: Option<&'j Progress>,
    responder: &'j Responder,
    connection: u64,
}

/// How a call is answered.
enum Answer {
    /// A result object's JSON text.
    Result(String),
    /// A JSON-RPC error.
    Error(ErrorObject),
    /// Nothing: the request was cancelled.
    Nothing,
}

/// The gate's verdict on a confirm-tier call.
enum Confirmation {
    /// The user accepted: run it.
    Granted,
    /// Stop here and answer so.
    Stop(Outcome, Answer),
}

/// Runs `job` to its answer.
pub(crate) fn run(job: Job) {
    let parser = job.shared.settings.limits.parser();
    let parsed = match &job.text {
        None => Ok(Value::Null),
        Some(raw) => raw.parse_with(&parser),
    };
    let mut seen: Option<(Arc<Entry>, Era, Option<String>)> = None;
    let (outcome, answer) = match &parsed {
        // rpc-io has validated this text within the same budgets.
        Err(_) => (
            Outcome::Failed,
            Answer::Error(ErrorObject::internal_error(
                "the call's params could not be read",
            )),
        ),
        Ok(params) => match prepare(&job, params) {
            Err(error) => (Outcome::Refused, Answer::Error(error)),
            Ok(call) => {
                let empty = Value::Object(jsonx::Object::new());
                let arguments = match params.get("arguments") {
                    Some(arguments @ Value::Object(_)) => arguments,
                    _ => &empty,
                };
                let answered = execute(&call, params, arguments);
                let client = call
                    .client_info
                    .as_ref()
                    .and_then(|info| info.get("name"))
                    .and_then(Value::as_str)
                    .map(|name| protocol::clip(name, 128).to_owned());
                seen = Some((Arc::clone(&call.entry), call.era, client));
                answered
            }
        },
    };
    let id = job.responder.id().clone();
    let sent = match answer {
        Answer::Result(text) => match json::raw(text) {
            Ok(raw) => job.responder.ok(raw),
            Err(error) => job.responder.err(error),
        },
        Answer::Error(error) => job.responder.err(error),
        Answer::Nothing => Ok(()),
    };
    // A request cancelled meanwhile (`AlreadyAnswered`) or a closed
    // connection (`Closed`) has no one waiting; MCP forbids answering the
    // former, and rpc-io has already dropped it.
    let _unanswerable = sent;
    let record = match &seen {
        Some((entry, era, client)) => AuditRecord {
            tool: Some(&entry.name),
            tier: Some(entry.tier),
            id: &id,
            era: Some(*era),
            client: client.as_deref(),
            outcome,
            elapsed: job.received.elapsed(),
        },
        None => AuditRecord {
            elapsed: job.received.elapsed(),
            outcome,
            ..AuditRecord::refused(&id)
        },
    };
    job.shared.audit(&record);
}

/// The checks of the request itself: era, `name`, `arguments`, the tool.
/// Their failures are protocol errors (`-32602`, `-32022`).
fn prepare<'j>(job: &'j Job, params: &Value<'_>) -> Result<Prepared<'j>, ErrorObject> {
    let legacy_version = job.legacy.as_ref().map(|l| l.version);
    let (era, version, capabilities, client_info) =
        match crate::session::served(params, legacy_version, job.shared.settings.legacy)? {
            crate::session::Served::Modern(m) => (
                Era::Modern,
                m.version,
                m.capabilities.clone().into_owned(),
                m.client_info.map(|info| info.clone().into_owned()),
            ),
            crate::session::Served::Legacy(version) => {
                let session = job.legacy.as_ref();
                (
                    Era::Legacy,
                    version,
                    session.map_or(Value::Null, |s| s.capabilities.clone()),
                    session.and_then(|s| s.client_info.clone()),
                )
            }
        };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err(ErrorObject::invalid_params(
            "`tools/call` needs `name` (a string)",
        ));
    };
    match params.get("arguments") {
        None | Some(Value::Null | Value::Object(_)) => {}
        Some(_) => return Err(ErrorObject::invalid_params("`arguments` must be an object")),
    }
    let Some(entry) = job.shared.registry.get(name) else {
        let message = format!("Unknown tool: {}", protocol::clip(name, 128));
        return Err(ErrorObject::invalid_params(message));
    };
    Ok(Prepared {
        shared: &job.shared,
        entry,
        text: job.text.as_ref(),
        era,
        version,
        capabilities,
        client_info,
        progress: job.progress.as_ref(),
        responder: &job.responder,
        connection: job.connection,
    })
}

fn execute(job: &Prepared<'_>, params: &Value<'_>, arguments: &Value<'_>) -> (Outcome, Answer) {
    if job.responder.is_cancelled() {
        return (Outcome::Cancelled, Answer::Nothing);
    }
    if let Err(violations) = job.entry.input.validate(arguments) {
        let report = violations.to_string();
        let shown = protocol::clip(&report, REPORT_CLIP);
        let more = if shown.len() < report.len() {
            " (shortened)"
        } else {
            ""
        };
        let message = format!(
            "Invalid arguments for the tool `{}`: {shown}{more}",
            job.entry.name
        );
        return (
            Outcome::InvalidArguments,
            Answer::Result(tool_error(job, message)),
        );
    }
    let request = PermissionRequest {
        tool: &job.entry.name,
        tier: job.entry.tier,
        arguments,
        client_info: job.client_info.as_ref(),
        era: job.era,
    };
    // The policy decides a call once. A 2026-07-28 retry that carries a
    // `requestState` was decided when the confirmation was asked for: it
    // goes to the confirmation, which runs it only if the ledger holds that
    // state for this tool and these arguments.
    let retry = job.era == Era::Modern && params.get("requestState").is_some();
    let decision = if retry {
        Decision::Confirm
    } else {
        catch_unwind(AssertUnwindSafe(|| job.shared.policy.decide(&request)))
            .unwrap_or_else(|_| Decision::deny("the server's permission policy failed"))
    };
    match permission::effective(job.entry.tier, decision) {
        Decision::Allow => {}
        Decision::Deny(reason) => {
            let message = format!("Permission denied: {reason}");
            return (Outcome::Denied, Answer::Result(tool_error(job, message)));
        }
        Decision::Confirm => match confirm(job, params, arguments) {
            Confirmation::Granted => {}
            Confirmation::Stop(outcome, answer) => return (outcome, answer),
        },
    }
    if job.responder.is_cancelled() {
        return (Outcome::Cancelled, Answer::Nothing);
    }
    let call = ToolCall {
        name: &job.entry.name,
        tier: job.entry.tier,
        arguments,
        id: job.responder.id(),
        era: job.era,
        version: job.version,
        capabilities: &job.capabilities,
        client_info: job.client_info.as_ref(),
        token: job.responder.token(),
        progress: job.progress,
        text: job.text.map_or(NO_PARAMS, Raw::as_str),
        parser: job.shared.settings.limits.parser(),
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| (job.entry.handler)(&call)));
    match outcome {
        Err(_panic) => (
            Outcome::Failed,
            Answer::Error(ErrorObject::internal_error("the tool failed unexpectedly")),
        ),
        Ok(Err(error)) => match error.kind {
            ToolErrorKind::Execution(message) => (
                Outcome::Completed { is_error: true },
                Answer::Result(tool_error(job, message)),
            ),
            ToolErrorKind::Protocol(error) => (Outcome::Failed, Answer::Error(error)),
        },
        Ok(Ok(result)) => finish(job, &result),
    }
}

/// Checks a handler's result against the output schema and renders it.
fn finish(job: &Prepared<'_>, result: &ToolResult) -> (Outcome, Answer) {
    if !result.is_error() {
        if let Some(schema) = &job.entry.output {
            let conforms = result
                .structured_content()
                .is_some_and(|s| schema.is_valid(s));
            if !conforms {
                // A server bug, not something the model can fix: MCP says
                // structured results MUST conform to the output schema.
                return (
                    Outcome::Failed,
                    Answer::Error(ErrorObject::internal_error(
                        "the tool's result does not match its output schema",
                    )),
                );
            }
        }
    }
    let mut o = Obj::with_capacity(256);
    result.write_members(&mut o, job.era == Era::Legacy);
    let text = job.shared.finish(o, job.era, "complete");
    (
        Outcome::Completed {
            is_error: result.is_error(),
        },
        Answer::Result(text),
    )
}

/// A tool execution error result (`isError: true`) with `message`.
fn tool_error(job: &Prepared<'_>, message: String) -> String {
    let mut o = Obj::with_capacity(message.len() + 96);
    ToolResult::error(message).write_members(&mut o, job.era == Era::Legacy);
    job.shared.finish(o, job.era, "complete")
}

/// Obtains the user's confirmation for a call, through the client.
///
/// The user is shown the call's arguments in full, never a part of them:
/// their canonical JSON with every non-ASCII character escaped (so no
/// look-alike or direction-changing character can disguise them), and the
/// confirmation is bound to exactly that text. Arguments whose shown form
/// would exceed `Server::with_max_confirmed_arguments` are refused instead
/// of shortened.
fn confirm(job: &Prepared<'_>, params: &Value<'_>, arguments: &Value<'_>) -> Confirmation {
    let refuse = |message: String| {
        Confirmation::Stop(
            Outcome::NotConfirmed,
            Answer::Result(tool_error(job, message)),
        )
    };
    let Ok(canonical) = crate::schema::canonical_text(arguments) else {
        return refuse(
            "Permission denied: the arguments are too deeply nested to show for confirmation."
                .to_owned(),
        );
    };
    let shown = shown_arguments(&canonical);
    let limit = job.shared.settings.max_confirmed_arguments;
    if shown.len() > limit {
        return refuse(format!(
            "This call needs the user's confirmation, and its arguments ({} bytes as shown) are \
             too large to show in full (the limit is {limit} bytes); the tool was not run. \
             Call it again with smaller arguments.",
            shown.len()
        ));
    }
    match job.era {
        Era::Modern => confirm_modern(job, params, shown),
        Era::Legacy => confirm_legacy(job, &shown),
    }
}

/// `canonical` (JSON text) with every non-ASCII character written as a
/// `\uXXXX` escape: the same JSON value, in text a person can read
/// unambiguously.
pub(crate) fn shown_arguments(canonical: &str) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(canonical.len());
    for c in canonical.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                let _infallible = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out
}

/// 2026-07-28: answer with an `InputRequiredResult`, or run on a retry that
/// carries a valid `requestState` and the user's acceptance.
fn confirm_modern(job: &Prepared<'_>, params: &Value<'_>, canonical: String) -> Confirmation {
    let name = &*job.entry.name;
    if let Some(state) = params.get("requestState") {
        let Some(state) = state.as_str() else {
            return stop_error(ErrorObject::invalid_params(
                "`requestState` must be a string",
            ));
        };
        match job
            .shared
            .ledger
            .redeem(state, name, job.entry.generation, &canonical)
        {
            Ok(()) => {}
            Err(Redeem::Unknown | Redeem::Mismatch) => {
                return stop_error(ErrorObject::invalid_params(
                    "the requestState is unknown, expired, already used, or belongs to another \
                     call; call the tool again without it",
                ));
            }
        }
        let response = params
            .get("inputResponses")
            .and_then(|r| r.get(CONFIRM_KEY));
        match read_answer(response) {
            Answer3::Accepted => return Confirmation::Granted,
            Answer3::Declined => {
                let message = "The user declined to run this tool call.".to_owned();
                return Confirmation::Stop(
                    Outcome::NotConfirmed,
                    Answer::Result(tool_error(job, message)),
                );
            }
            // No usable answer: ask again (MCP: re-request missing input
            // rather than fail).
            Answer3::Missing => {}
        }
    }
    if !protocol::supports_form_elicitation(&job.capabilities) {
        let required = jsonx::json!({ "elicitation": { "form": {} } });
        return stop_error(protocol::missing_capability(&required));
    }
    let message = question(job, &canonical);
    let token = job
        .shared
        .ledger
        .issue(job.connection, name, job.entry.generation, &canonical);
    let mut o = Obj::with_capacity(message.len() + 512);
    let _ = o
        .with("inputRequests", |out| {
            let mut requests = Obj::new();
            let _ = requests.raw(CONFIRM_KEY, &elicitation(&message, true));
            out.push_str(&requests.finish());
        })
        .str("requestState", &token);
    let text = job.shared.finish(o, Era::Modern, "input_required");
    Confirmation::Stop(Outcome::ConfirmationRequested, Answer::Result(text))
}

/// 2025-11-25 and 2025-06-18: send `elicitation/create` to the client and
/// wait for its answer, or for the `tools/call` to be cancelled.
fn confirm_legacy(job: &Prepared<'_>, canonical: &str) -> Confirmation {
    let refuse = |message: &str| {
        Confirmation::Stop(
            Outcome::NotConfirmed,
            Answer::Result(tool_error(job, message.to_owned())),
        )
    };
    if !protocol::supports_form_elicitation(&job.capabilities) {
        return refuse(
            "This tool needs the user's confirmation, and the client did not declare the \
             elicitation capability, so it cannot be asked; the tool was not run.",
        );
    }
    if job.shared.settings.workers == 0 {
        // The answer would arrive on the thread that is running this call.
        return refuse(
            "This tool needs the user's confirmation, which this server cannot wait for \
             (it runs tool calls without worker threads); the tool was not run.",
        );
    }
    let message = question(job, canonical);
    let Ok(params) = json::raw(elicitation(&message, false)) else {
        return refuse("The confirmation request could not be built; the tool was not run.");
    };
    let peer = job.responder.peer();
    let timeout = job.shared.settings.confirmation_timeout;
    // A timeout too large to add to the clock means no deadline.
    let sent = if Instant::now().checked_add(timeout).is_some() {
        peer.request_with_timeout("elicitation/create", params, timeout)
    } else {
        peer.request("elicitation/create", params)
    };
    let Ok(call) = sent else {
        return refuse("The confirmation could not be requested; the tool was not run.");
    };
    match wait_unless_cancelled(call, &job.responder.token()) {
        None => Confirmation::Stop(Outcome::Cancelled, Answer::Nothing),
        Some(Ok(raw)) => match raw.parse() {
            Ok(answer) if matches!(read_answer(Some(&answer)), Answer3::Accepted) => {
                Confirmation::Granted
            }
            _ => refuse("The user declined to run this tool call."),
        },
        Some(Err(CallError::TimedOut)) => {
            refuse("The user did not answer the confirmation in time; the tool was not run.")
        }
        Some(Err(_)) => refuse("The confirmation was not answered; the tool was not run."),
    }
}

/// Waits for `call`'s outcome, checking `cancelled` at least every 50 ms;
/// `None` if it was set first. The call is then dropped, which cancels the
/// elicitation with the client.
fn wait_unless_cancelled(
    call: rpc_io::Call,
    cancelled: &CancelToken,
) -> Option<Result<rpc_io::Raw<'static>, CallError>> {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut call = Box::pin(call);
    loop {
        if let std::task::Poll::Ready(outcome) = call.as_mut().poll(&mut context) {
            return Some(outcome);
        }
        if cancelled.is_cancelled() {
            return None;
        }
        std::thread::park_timeout(std::time::Duration::from_millis(50));
    }
}

fn stop_error(error: ErrorObject) -> Confirmation {
    Confirmation::Stop(Outcome::NotConfirmed, Answer::Error(error))
}

/// What the user said to a confirmation.
enum Answer3 {
    Accepted,
    Declined,
    Missing,
}

/// Reads an `ElicitResult`: `{"action": "accept", "content": {"confirm": true}}`
/// accepts; `decline`, `cancel`, or `confirm: false` declines; anything
/// else is no answer.
fn read_answer(response: Option<&Value<'_>>) -> Answer3 {
    let Some(response) = response else {
        return Answer3::Missing;
    };
    match response.get("action").and_then(Value::as_str) {
        Some("accept") => match response
            .get("content")
            .and_then(|c| c.get(CONFIRM_KEY))
            .and_then(Value::as_bool)
        {
            Some(true) => Answer3::Accepted,
            Some(false) => Answer3::Declined,
            None => Answer3::Missing,
        },
        Some("decline" | "cancel") => Answer3::Declined,
        _ => Answer3::Missing,
    }
}

/// The question put to the user: the tool's own wording, then what will run.
/// The question put to the user: the tool's own wording, then exactly what
/// will run (`shown`: the full arguments, see [`confirm`]).
fn question(job: &Prepared<'_>, shown: &str) -> String {
    let asked = job
        .entry
        .confirmation
        .as_deref()
        .unwrap_or("Allow the tool to run?");
    format!(
        "{asked}\n\nTool: {} ({} tier)\nArguments: {shown}",
        job.entry.name, job.entry.tier
    )
}

/// An `elicitation/create` request's params (`with_method`: wrapped as an
/// `inputRequests` entry, `{method, params}`).
fn elicitation(message: &str, with_method: bool) -> String {
    let mut params = Obj::with_capacity(message.len() + 256);
    if with_method {
        let _ = params.str("mode", "form");
    }
    let _ = params.str("message", message).raw(
        "requestedSchema",
        r#"{"type":"object","properties":{"confirm":{"type":"boolean","title":"Allow this call","description":"Check to let the tool run with the arguments shown.","default":false}},"required":["confirm"]}"#,
    );
    let params = params.finish();
    if !with_method {
        return params;
    }
    let mut request = Obj::with_capacity(params.len() + 48);
    let _ = request
        .str("method", "elicitation/create")
        .raw("params", &params);
    request.finish()
}

/// One tool call, as its handler sees it.
///
/// Gives the validated arguments, who is calling (self-reported), the
/// cancellation flag, and progress reports. It lives only while the handler
/// runs, so nothing can be sent for the call after its answer.
///
/// # Examples
///
/// ```
/// use mcp_io::{Implementation, Server, Tier, Tool, ToolResult, jsonx::json};
///
/// let server = Server::new(Implementation::new("demo", "0.1.0")).with_workers(0).tool(
///     Tool::new("count", Tier::ReadOnly).input_schema(json!({
///         "type": "object",
///         "properties": { "to": { "type": "integer", "minimum": 0, "maximum": 1000 } },
///         "required": ["to"]
///     })),
///     |call| {
///         let to = call.arguments()["to"].as_u64().unwrap_or(0);
///         for i in 1..=to {
///             if call.is_cancelled() {
///                 break;
///             }
///             call.progress(i as f64, Some(to as f64), None)?;
///         }
///         Ok(ToolResult::text(format!("counted to {to}")))
///     },
/// )?;
/// # let _ = server;
/// # Ok::<(), mcp_io::RegisterError>(())
/// ```
pub struct ToolCall<'a> {
    name: &'a str,
    tier: Tier,
    arguments: &'a Value<'a>,
    id: &'a Id<'static>,
    era: Era,
    version: &'static str,
    capabilities: &'a Value<'a>,
    client_info: Option<&'a Value<'a>>,
    token: CancelToken,
    progress: Option<&'a Progress>,
    /// The request's `params` text, which typed arguments borrow from.
    text: &'a str,
    /// The endpoint's JSON budgets.
    parser: jsonx::Parser,
}

impl<'a> ToolCall<'a> {
    /// The tool's name.
    #[must_use]
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// The tool's tier.
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// The arguments: an object, valid against the tool's input schema
    /// (`{}` when the client sent none).
    #[must_use]
    pub fn arguments(&self) -> &'a Value<'a> {
        self.arguments
    }

    /// The arguments as a typed value, any jsonx-lib
    /// [`FromJson`](crate::jsonx::FromJson) type, read from the request's
    /// own text: a type that borrows (`&'a str`, `Cow<'a, str>`) borrows from
    /// the message, with no copy. Absent arguments read as `{}`. The
    /// arguments have passed the tool's input schema already; this is the
    /// typed view of them, under the endpoint's JSON budgets.
    ///
    /// # Errors
    ///
    /// A tool execution error (`isError: true`, so the model can fix its
    /// call) naming where the arguments do not fit `T`: the type asks for
    /// more than the schema does.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Implementation, Server, Tier, Tool, ToolResult, jsonx::{self, json}};
    ///
    /// struct Greet<'a> { name: &'a str, times: u8 }
    ///
    /// impl<'a> jsonx::FromJson<'a> for Greet<'a> {
    ///     fn from_json(reader: &mut jsonx::Reader<'a>) -> Result<Self, jsonx::DecodeError> {
    ///         let (mut name, mut times) = (None, None);
    ///         let mut object = reader.read_object()?;
    ///         while let Some(key) = object.next_key()? {
    ///             match &*key {
    ///                 "name" => object.field(&mut name)?,
    ///                 "times" => object.field(&mut times)?,
    ///                 _ => object.skip()?,
    ///             }
    ///         }
    ///         Ok(Greet { name: object.required(name, "name")?, times: times.unwrap_or(1) })
    ///     }
    /// }
    ///
    /// let server = Server::new(Implementation::new("demo", "1.0.0")).tool(
    ///     Tool::new("greet", Tier::ReadOnly).input_schema(json!({
    ///         "type": "object",
    ///         "properties": { "name": { "type": "string" }, "times": { "type": "integer", "minimum": 1, "maximum": 3 } },
    ///         "required": ["name"]
    ///     })),
    ///     |call| {
    ///         let greet: Greet<'_> = call.decode()?; // `name` borrows from the request
    ///         Ok(ToolResult::text(vec![format!("Hello, {}!", greet.name); usize::from(greet.times)].join(" ")))
    ///     },
    /// )?;
    /// # let _ = server;
    /// # Ok::<(), mcp_io::RegisterError>(())
    /// ```
    pub fn decode<T: jsonx::FromJson<'a>>(&self) -> Result<T, ToolError> {
        self.parser
            .decode::<ArgumentsOf<T>>(self.text)
            .map(|of| of.0)
            .map_err(|error| {
                let message = error.to_string();
                ToolError::new(format!(
                    "The arguments do not fit the tool `{}`: {}",
                    self.name,
                    protocol::clip(&message, 512)
                ))
            })
    }

    /// The request's JSON-RPC id.
    #[must_use]
    pub fn id(&self) -> &'a Id<'static> {
        self.id
    }

    /// The era the call is served under.
    #[must_use]
    pub fn era(&self) -> Era {
        self.era
    }

    /// The protocol version: the request's own (modern), or the session's
    /// negotiated one (legacy).
    #[must_use]
    pub fn protocol_version(&self) -> &'static str {
        self.version
    }

    /// The client's declared capabilities: the request's own (modern), or
    /// those declared at `initialize` (legacy). Check them before relying on
    /// a capability, and answer [`ToolError::missing_capability`](crate::ToolError::missing_capability)
    /// when one is missing.
    #[must_use]
    pub fn client_capabilities(&self) -> &'a Value<'a> {
        self.capabilities
    }

    /// The client's self-reported `clientInfo`, if any: for display and
    /// logs only, never for a security decision.
    #[must_use]
    pub fn client_info(&self) -> Option<&'a Value<'a>> {
        self.client_info
    }

    /// Whether the client cancelled the call (or the connection closed).
    /// Long handlers should check it and stop: the answer would be dropped
    /// anyway.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// The cancellation flag, to hand to work running elsewhere.
    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        self.token.clone()
    }

    /// Whether the client asked for progress (`_meta.progressToken`).
    #[must_use]
    pub fn wants_progress(&self) -> bool {
        self.progress.is_some()
    }

    /// Reports progress: `progress` so far, out of `total` if known, with an
    /// optional message. Does nothing (and succeeds) when the client asked
    /// for no progress.
    ///
    /// # Errors
    ///
    /// [`Error::ProgressNotIncreasing`] for a value not above the last one
    /// (MCP: progress "MUST increase"); [`Error::Cancelled`] once the call
    /// was cancelled (MCP forbids sending anything more for it);
    /// [`Error::Rpc`] for a value that is not finite or a closed connection.
    /// Nothing is sent in any of these cases.
    ///
    /// # Examples
    ///
    /// See [`ToolCall`].
    pub fn progress(
        &self,
        progress: f64,
        total: Option<f64>,
        message: Option<&str>,
    ) -> Result<(), Error> {
        let Some(reporter) = self.progress else {
            return Ok(());
        };
        if self.is_cancelled() {
            return Err(Error::Cancelled);
        }
        // rpc-io's request-bound progress refuses a value that does not
        // increase, and any report once the request is answered or
        // cancelled, under the lock its response is written under.
        match reporter.report(progress, total, message) {
            Ok(()) => Ok(()),
            Err(rpc_io::Error::AlreadyAnswered) => Err(Error::Cancelled),
            Err(rpc_io::Error::ProgressNotIncreasing { last, next }) => {
                Err(Error::ProgressNotIncreasing { last, next })
            }
            Err(error) => Err(Error::Rpc(error)),
        }
    }
}

/// The `arguments` member of a `tools/call`'s params, read as `T` straight
/// from the reader (other members skipped; absent arguments read as `{}`).
struct ArgumentsOf<T>(T);

impl<'de, T: jsonx::FromJson<'de>> jsonx::FromJson<'de> for ArgumentsOf<T> {
    fn from_json(reader: &mut jsonx::Reader<'de>) -> Result<Self, jsonx::DecodeError> {
        let mut found = None;
        let mut object = reader.read_object()?;
        while let Some(key) = object.next_key()? {
            if key == "arguments" {
                found = Some(object.value::<T>()?);
            } else {
                object.skip()?;
            }
        }
        match found {
            Some(value) => Ok(Self(value)),
            None => jsonx::from_str::<T>(NO_PARAMS).map(Self),
        }
    }
}

impl core::fmt::Debug for ToolCall<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ToolCall")
            .field("name", &self.name)
            .field("tier", &self.tier)
            .field("id", &self.id)
            .field("era", &self.era)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonx::json;

    #[test]
    fn test_read_answer() {
        let accepted = json!({ "action": "accept", "content": { "confirm": true } });
        assert!(matches!(read_answer(Some(&accepted)), Answer3::Accepted));
        for declined in [
            json!({ "action": "accept", "content": { "confirm": false } }),
            json!({ "action": "decline" }),
            json!({ "action": "cancel" }),
        ] {
            assert!(matches!(read_answer(Some(&declined)), Answer3::Declined));
        }
        for missing in [
            json!({}),
            json!({ "action": "accept" }),
            json!({ "action": "maybe" }),
            json!(1),
        ] {
            assert!(matches!(read_answer(Some(&missing)), Answer3::Missing));
        }
        assert!(matches!(read_answer(None), Answer3::Missing));
    }

    #[test]
    fn test_shown_arguments_escape_everything_but_ascii() {
        assert_eq!(shown_arguments(r#"{"a":"x"}"#), r#"{"a":"x"}"#);
        assert_eq!(
            shown_arguments("\"caf\u{e9}\u{202e}\u{1F600}\""),
            r#""caf\u00e9\u202e\ud83d\ude00""#
        );
        let shown = shown_arguments("{\"k\u{e9}\":\"v\u{1F600}\"}");
        assert_eq!(
            jsonx::parse(&shown).ok(),
            jsonx::parse("{\"k\u{e9}\":\"v\u{1F600}\"}").ok()
        );
    }

    #[test]
    fn test_elicitation_shapes() {
        let wrapped = elicitation("ok?", true);
        let parsed = jsonx::parse(&wrapped).ok();
        let parsed = parsed.as_ref();
        assert_eq!(
            parsed.map(|v| v["method"] == "elicitation/create"),
            Some(true)
        );
        assert_eq!(parsed.map(|v| v["params"]["mode"] == "form"), Some(true));
        let plain = elicitation("ok?", false);
        assert!(plain.starts_with(r#"{"message":"ok?","requestedSchema":"#));
    }
}
