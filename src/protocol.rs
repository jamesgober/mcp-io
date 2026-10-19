//! Protocol revisions, reserved `_meta` keys, error codes, and how each
//! request's era is told apart.
//!
//! MCP 2026-07-28 is stateless: every request names its protocol version and
//! the client's capabilities in `params._meta`, and the server answers each
//! request on its own. Revisions up to 2025-11-25 opened a session with an
//! `initialize` handshake instead. This crate serves both ("dual-era", in the
//! spec's words): a request that carries
//! `_meta["io.modelcontextprotocol/protocolVersion"]` is served statelessly,
//! and a connection that opens with `initialize` is served with the
//! negotiated legacy revision's semantics.

use crate::{
    jsonx::Value,
    rpc_io::{ErrorCode, ErrorObject},
};

/// The protocol revision this crate implements first: MCP 2026-07-28.
pub const LATEST_PROTOCOL_VERSION: &str = "2026-07-28";

/// The revisions served statelessly, through per-request `_meta`, newest
/// first. These are the versions `server/discover` and
/// `UnsupportedProtocolVersion` errors name.
pub const MODERN_PROTOCOL_VERSIONS: &[&str] = &["2026-07-28"];

/// The handshake-based revisions an `initialize` request can negotiate,
/// newest first. 2025-03-26 and earlier are not served: they require JSON-RPC
/// batches, which 2025-06-18 removed and this crate refuses. A client asking
/// for one is offered 2025-11-25, as the handshake prescribes.
pub const LEGACY_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18"];

/// `-32022`: the request's protocol version is not supported. The error's
/// `data` is `{"supported": [...], "requested": "..."}`.
pub const UNSUPPORTED_PROTOCOL_VERSION: ErrorCode = ErrorCode(-32022);

/// `-32021`: processing the request needs a client capability the request
/// did not declare. The error's `data` is `{"requiredCapabilities": {...}}`.
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: ErrorCode = ErrorCode(-32021);

/// `-32020`: an HTTP request's headers do not match its body. Defined for the
/// Streamable HTTP transport; never sent on stdio.
pub const HEADER_MISMATCH: ErrorCode = ErrorCode(-32020);

/// The `_meta` keys MCP reserves that this crate reads or writes.
pub mod meta {
    /// The protocol version of one request (required on modern requests).
    pub const PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
    /// The client's capabilities for one request (required on modern
    /// requests).
    pub const CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
    /// The client's self-reported name and version (optional, display only).
    pub const CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
    /// The log level a request asks for (deprecated with the Logging feature).
    pub const LOG_LEVEL: &str = "io.modelcontextprotocol/logLevel";
    /// The server's name and version, in every modern result's `_meta`.
    pub const SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
    /// The `subscriptions/listen` request a notification belongs to.
    pub const SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";
    /// Opts a request into progress notifications.
    pub const PROGRESS_TOKEN: &str = "progressToken";
}

/// Which protocol era a request is served under.
///
/// # Examples
///
/// ```
/// use mcp_io::protocol::Era;
///
/// assert_eq!(Era::Modern.to_string(), "modern (stateless, per-request _meta)");
/// assert_ne!(Era::Modern, Era::Legacy);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Era {
    /// Stateless: the request carried its version and capabilities in
    /// `_meta` (2026-07-28).
    Modern,
    /// Session-based: the connection opened with `initialize` (2025-11-25 or
    /// 2025-06-18), and the request relies on that session.
    Legacy,
}

impl core::fmt::Display for Era {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Modern => "modern (stateless, per-request _meta)",
            Self::Legacy => "legacy (initialize handshake)",
        })
    }
}

/// The error for a request whose version this server does not serve.
///
/// # Examples
///
/// ```
/// let error = mcp_io::protocol::unsupported_version("1900-01-01");
/// assert_eq!(error.code(), mcp_io::protocol::UNSUPPORTED_PROTOCOL_VERSION);
/// assert_eq!(
///     error.data().map(|d| d.as_str()),
///     Some(r#"{"supported":["2026-07-28"],"requested":"1900-01-01"}"#),
/// );
/// ```
#[must_use]
pub fn unsupported_version(requested: &str) -> ErrorObject {
    let mut data = String::from("{\"supported\":[");
    for (i, version) in MODERN_PROTOCOL_VERSIONS.iter().enumerate() {
        if i > 0 {
            data.push(',');
        }
        crate::jsonx::write_string(&mut data, version);
    }
    data.push_str("],\"requested\":");
    crate::jsonx::write_string(&mut data, clip(requested, 128));
    data.push('}');
    let error = ErrorObject::new(UNSUPPORTED_PROTOCOL_VERSION, "Unsupported protocol version");
    match crate::rpc_io::Raw::from_string(data) {
        Ok(raw) => error.with_data(raw),
        Err(_) => error,
    }
}

/// The error for a request that needs client capabilities it did not
/// declare. `required` is a `ClientCapabilities` object naming them (for
/// form-mode elicitation: `{"elicitation": {"form": {}}}`).
///
/// # Examples
///
/// ```
/// use mcp_io::{jsonx::json, protocol};
///
/// let error = protocol::missing_capability(&json!({ "elicitation": { "form": {} } }));
/// assert_eq!(error.code(), protocol::MISSING_REQUIRED_CLIENT_CAPABILITY);
/// assert_eq!(
///     error.data().map(|d| d.as_str()),
///     Some(r#"{"requiredCapabilities":{"elicitation":{"form":{}}}}"#),
/// );
/// ```
#[must_use]
pub fn missing_capability(required: &Value<'_>) -> ErrorObject {
    let mut data = String::from("{\"requiredCapabilities\":");
    crate::jsonx::write_value(&mut data, required);
    data.push('}');
    let error = ErrorObject::new(
        MISSING_REQUIRED_CLIENT_CAPABILITY,
        "Missing required client capability",
    );
    match crate::rpc_io::Raw::from_string(data) {
        Ok(raw) => error.with_data(raw),
        Err(_) => error,
    }
}

/// The eight syslog levels MCP's (deprecated) Logging feature names.
const LOG_LEVELS: &[&str] = &[
    "debug",
    "info",
    "notice",
    "warning",
    "error",
    "critical",
    "alert",
    "emergency",
];

/// What a request's `_meta` says about its era.
pub(crate) enum Classified<'v> {
    /// Served statelessly.
    Modern(Modern<'v>),
    /// No `protocolVersion`: legacy semantics, if a session exists.
    Unversioned,
}

/// The per-request fields of a modern request.
pub(crate) struct Modern<'v> {
    pub(crate) version: &'static str,
    pub(crate) capabilities: &'v Value<'v>,
    pub(crate) client_info: Option<&'v Value<'v>>,
}

/// Reads `params._meta` (`params` already parsed; `Null` when absent).
///
/// # Errors
///
/// `-32602` for params that are not an object, a `_meta` that is not one,
/// a malformed reserved field, or a modern request missing
/// `clientCapabilities`; `-32022` for a version this server does not serve.
pub(crate) fn classify<'v>(params: &'v Value<'v>) -> Result<Classified<'v>, ErrorObject> {
    let meta = match params {
        Value::Null => return Ok(Classified::Unversioned),
        Value::Object(object) => match object.get("_meta") {
            None | Some(Value::Null) => return Ok(Classified::Unversioned),
            Some(Value::Object(meta)) => meta,
            Some(_) => return Err(ErrorObject::invalid_params("`_meta` must be an object")),
        },
        _ => return Err(ErrorObject::invalid_params("params must be an object")),
    };
    let version = match meta.get(meta::PROTOCOL_VERSION) {
        None => return Ok(Classified::Unversioned),
        Some(Value::String(v)) => v,
        Some(_) => {
            return Err(ErrorObject::invalid_params(
                "`_meta.io.modelcontextprotocol/protocolVersion` must be a string",
            ));
        }
    };
    let Some(&version) = MODERN_PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == version.as_ref())
    else {
        return Err(unsupported_version(version));
    };
    let capabilities = match meta.get(meta::CLIENT_CAPABILITIES) {
        Some(c @ Value::Object(_)) => c,
        Some(_) => {
            return Err(ErrorObject::invalid_params(
                "`_meta.io.modelcontextprotocol/clientCapabilities` must be an object",
            ));
        }
        None => {
            return Err(ErrorObject::invalid_params(
                "missing required `_meta.io.modelcontextprotocol/clientCapabilities`",
            ));
        }
    };
    let client_info = match meta.get(meta::CLIENT_INFO) {
        None => None,
        Some(info @ Value::Object(_)) => Some(info),
        Some(_) => {
            return Err(ErrorObject::invalid_params(
                "`_meta.io.modelcontextprotocol/clientInfo` must be an object",
            ));
        }
    };
    if let Some(level) = meta.get(meta::LOG_LEVEL) {
        if !level.as_str().is_some_and(|l| LOG_LEVELS.contains(&l)) {
            return Err(ErrorObject::invalid_params(
                "`_meta.io.modelcontextprotocol/logLevel` is not a recognized log level",
            ));
        }
    }
    Ok(Classified::Modern(Modern {
        version,
        capabilities,
        client_info,
    }))
}

/// Whether a `ClientCapabilities` object declares form-mode elicitation:
/// `elicitation: {}` (form only, for backward compatibility) or
/// `elicitation: {"form": ...}`.
pub(crate) fn supports_form_elicitation(capabilities: &Value<'_>) -> bool {
    match capabilities.get("elicitation") {
        Some(Value::Object(modes)) => modes.is_empty() || modes.contains_key("form"),
        _ => false,
    }
}

/// `text` cut to at most `max` bytes, on a character boundary.
pub(crate) fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{jsonx::json, rpc_io::ErrorCode};

    fn code(params: &Value<'_>) -> Option<ErrorCode> {
        classify(params).err().map(|e| e.code())
    }

    #[test]
    fn test_classify_modern_request_reads_every_field() {
        let params = json!({ "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": { "elicitation": {} },
            "io.modelcontextprotocol/clientInfo": { "name": "c", "version": "1" },
            "io.modelcontextprotocol/logLevel": "info",
            "traceparent": "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01"
        } });
        let Ok(Classified::Modern(m)) = classify(&params) else {
            panic!()
        };
        assert_eq!(m.version, "2026-07-28");
        assert!(supports_form_elicitation(m.capabilities));
        assert_eq!(m.client_info.unwrap()["name"], "c");
    }

    #[test]
    fn test_classify_without_version_is_unversioned() {
        for params in [
            json!(null),
            json!({}),
            json!({ "_meta": null }),
            json!({ "_meta": { "progressToken": 1 } }),
        ] {
            assert!(matches!(classify(&params), Ok(Classified::Unversioned)));
        }
    }

    #[test]
    fn test_classify_rejects_malformed_meta_with_invalid_params() {
        let invalid = ErrorCode::INVALID_PARAMS;
        assert_eq!(code(&json!([1])), Some(invalid));
        assert_eq!(code(&json!({ "_meta": 1 })), Some(invalid));
        assert_eq!(
            code(&json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": 2026 } })),
            Some(invalid)
        );
        assert_eq!(
            code(&json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" } })),
            Some(invalid)
        );
        let base = |extra: (&'static str, Value<'static>)| {
            let mut meta = crate::jsonx::Object::new();
            meta.push("io.modelcontextprotocol/protocolVersion", "2026-07-28");
            meta.push("io.modelcontextprotocol/clientCapabilities", json!({}));
            meta.push(extra.0, extra.1);
            json!({ "_meta": Value::Object(meta) })
        };
        let caps = json!({ "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": []
        } });
        assert_eq!(code(&caps), Some(invalid));
        assert_eq!(
            code(&base(("io.modelcontextprotocol/clientInfo", json!("me")))),
            Some(invalid)
        );
        assert_eq!(
            code(&base((
                "io.modelcontextprotocol/logLevel",
                json!("verbose")
            ))),
            Some(invalid)
        );
        assert_eq!(
            code(&base(("io.modelcontextprotocol/logLevel", json!(3)))),
            Some(invalid)
        );
    }

    #[test]
    fn test_classify_unknown_version_is_unsupported_with_supported_list() {
        let params = json!({ "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2025-11-25",
            "io.modelcontextprotocol/clientCapabilities": {}
        } });
        let error = classify(&params).err().unwrap();
        assert_eq!(error.code(), UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(error.message(), "Unsupported protocol version");
        assert_eq!(
            error.data().unwrap().as_str(),
            r#"{"supported":["2026-07-28"],"requested":"2025-11-25"}"#
        );
    }

    #[test]
    fn test_unsupported_version_clips_a_huge_request() {
        let error = unsupported_version(&"9".repeat(10_000));
        assert!(error.data().unwrap().as_str().len() < 200);
    }

    #[test]
    fn test_form_elicitation_detection() {
        assert!(supports_form_elicitation(&json!({ "elicitation": {} })));
        assert!(supports_form_elicitation(
            &json!({ "elicitation": { "form": {}, "url": {} } })
        ));
        assert!(!supports_form_elicitation(
            &json!({ "elicitation": { "url": {} } })
        ));
        assert!(!supports_form_elicitation(&json!({ "sampling": {} })));
        assert!(!supports_form_elicitation(&json!({ "elicitation": true })));
    }

    #[test]
    fn test_clip_respects_char_boundaries() {
        assert_eq!(clip("héllo", 2), "h");
        assert_eq!(clip("abc", 10), "abc");
    }

    #[test]
    fn test_reserved_codes_are_in_the_spec_range() {
        for code in [
            UNSUPPORTED_PROTOCOL_VERSION,
            MISSING_REQUIRED_CLIENT_CAPABILITY,
            HEADER_MISMATCH,
        ] {
            assert!((-32099..=-32020).contains(&code.0));
        }
    }
}
