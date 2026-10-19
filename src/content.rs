//! What a tool returns: content blocks, structured content, and tool errors.

use core::fmt;

use crate::{
    json::Obj,
    jsonx::{self, Value},
    rpc_io::ErrorObject,
};

/// One content block of a tool result (MCP's `ContentBlock`).
///
/// Binary data (`Image`, `Audio`, a blob resource) is base64 text; the
/// `*_bytes` constructors encode raw bytes.
///
/// # Examples
///
/// ```
/// use mcp_io::Content;
///
/// let text = Content::text("3 matches");
/// let png = Content::image_bytes(&[0x89, b'P', b'N', b'G'], "image/png");
/// let link = Content::resource_link("file:///notes/a.md", "a.md").with_mime_type("text/markdown");
/// assert_eq!(text.to_json(), r#"{"type":"text","text":"3 matches"}"#);
/// assert_eq!(png.to_json(), r#"{"type":"image","data":"iVBORw==","mimeType":"image/png"}"#);
/// assert_eq!(link.to_json(), r#"{"type":"resource_link","uri":"file:///notes/a.md","name":"a.md","mimeType":"text/markdown"}"#);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Content {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// An image.
    Image {
        /// Base64 image data.
        data: String,
        /// The image's MIME type.
        mime_type: String,
    },
    /// Audio.
    Audio {
        /// Base64 audio data.
        data: String,
        /// The audio's MIME type.
        mime_type: String,
    },
    /// A link to a resource the client may fetch.
    ResourceLink {
        /// The resource's URI.
        uri: String,
        /// Its name.
        name: String,
        /// Its MIME type, if known.
        mime_type: Option<String>,
        /// What it is, if described.
        description: Option<String>,
    },
    /// A resource's contents, embedded.
    Resource {
        /// The resource's URI.
        uri: String,
        /// Its MIME type, if known.
        mime_type: Option<String>,
        /// Text, or base64 binary data.
        contents: ResourceContents,
    },
}

/// The contents of an embedded resource.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResourceContents {
    /// Text.
    Text(String),
    /// Base64 binary data.
    Blob(String),
}

impl Content {
    /// A text block.
    ///
    /// # Examples
    ///
    /// ```
    /// assert!(matches!(mcp_io::Content::text("hi"), mcp_io::Content::Text { .. }));
    /// ```
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// An image from base64 `data`.
    ///
    /// # Examples
    ///
    /// ```
    /// let image = mcp_io::Content::image("iVBORw==", "image/png");
    /// assert!(image.to_json().contains("\"type\":\"image\""));
    /// ```
    #[must_use]
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    /// An image from raw bytes, base64-encoded here.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::Content::image_bytes(b"hi", "image/png"), mcp_io::Content::image("aGk=", "image/png"));
    /// ```
    #[must_use]
    pub fn image_bytes(bytes: &[u8], mime_type: impl Into<String>) -> Self {
        Self::image(base64(bytes), mime_type)
    }

    /// Audio from base64 `data`.
    ///
    /// # Examples
    ///
    /// ```
    /// let audio = mcp_io::Content::audio("UklGRg==", "audio/wav");
    /// assert!(audio.to_json().contains("\"type\":\"audio\""));
    /// ```
    #[must_use]
    pub fn audio(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Audio {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    /// Audio from raw bytes, base64-encoded here.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::Content::audio_bytes(b"RIFF", "audio/wav"), mcp_io::Content::audio("UklGRg==", "audio/wav"));
    /// ```
    #[must_use]
    pub fn audio_bytes(bytes: &[u8], mime_type: impl Into<String>) -> Self {
        Self::audio(base64(bytes), mime_type)
    }

    /// A link to the resource at `uri`.
    ///
    /// # Examples
    ///
    /// ```
    /// let link = mcp_io::Content::resource_link("file:///a.rs", "a.rs").with_description("entry point");
    /// assert!(link.to_json().ends_with(r#""description":"entry point"}"#));
    /// ```
    #[must_use]
    pub fn resource_link(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self::ResourceLink {
            uri: uri.into(),
            name: name.into(),
            mime_type: None,
            description: None,
        }
    }

    /// A resource's text, embedded.
    ///
    /// # Examples
    ///
    /// ```
    /// let doc = mcp_io::Content::embedded_text("file:///a.md", "# A").with_mime_type("text/markdown");
    /// assert_eq!(doc.to_json(), r##"{"type":"resource","resource":{"uri":"file:///a.md","mimeType":"text/markdown","text":"# A"}}"##);
    /// ```
    #[must_use]
    pub fn embedded_text(uri: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Resource {
            uri: uri.into(),
            mime_type: None,
            contents: ResourceContents::Text(text.into()),
        }
    }

    /// A resource's bytes, embedded (base64-encoded here).
    ///
    /// # Examples
    ///
    /// ```
    /// let blob = mcp_io::Content::embedded_bytes("file:///a.bin", b"\x00\x01");
    /// assert_eq!(blob.to_json(), r#"{"type":"resource","resource":{"uri":"file:///a.bin","blob":"AAE="}}"#);
    /// ```
    #[must_use]
    pub fn embedded_bytes(uri: impl Into<String>, bytes: &[u8]) -> Self {
        Self::Resource {
            uri: uri.into(),
            mime_type: None,
            contents: ResourceContents::Blob(base64(bytes)),
        }
    }

    /// Sets the MIME type of a resource link or an embedded resource (other
    /// blocks are returned unchanged; images and audio take theirs in the
    /// constructor).
    ///
    /// # Examples
    ///
    /// ```
    /// let text = mcp_io::Content::text("x").with_mime_type("text/plain");
    /// assert_eq!(text, mcp_io::Content::text("x"));
    /// ```
    #[must_use]
    pub fn with_mime_type(mut self, mime: impl Into<String>) -> Self {
        match &mut self {
            Self::ResourceLink { mime_type, .. } | Self::Resource { mime_type, .. } => {
                *mime_type = Some(mime.into());
            }
            Self::Text { .. } | Self::Image { .. } | Self::Audio { .. } => {}
        }
        self
    }

    /// Sets the description of a resource link (other blocks are returned
    /// unchanged).
    ///
    /// # Examples
    ///
    /// See [`resource_link`](Self::resource_link).
    #[must_use]
    pub fn with_description(mut self, text: impl Into<String>) -> Self {
        if let Self::ResourceLink { description, .. } = &mut self {
            *description = Some(text.into());
        }
        self
    }

    /// The block as MCP JSON text.
    ///
    /// # Examples
    ///
    /// See [`Content`].
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut o = Obj::new();
        match self {
            Self::Text { text } => {
                let _ = o.str("type", "text").str("text", text);
            }
            Self::Image { data, mime_type } => {
                let _ = o
                    .str("type", "image")
                    .str("data", data)
                    .str("mimeType", mime_type);
            }
            Self::Audio { data, mime_type } => {
                let _ = o
                    .str("type", "audio")
                    .str("data", data)
                    .str("mimeType", mime_type);
            }
            Self::ResourceLink {
                uri,
                name,
                mime_type,
                description,
            } => {
                let _ = o
                    .str("type", "resource_link")
                    .str("uri", uri)
                    .str("name", name)
                    .opt_str("mimeType", mime_type.as_deref())
                    .opt_str("description", description.as_deref());
            }
            Self::Resource {
                uri,
                mime_type,
                contents,
            } => {
                let mut r = Obj::new();
                let _ = r.str("uri", uri).opt_str("mimeType", mime_type.as_deref());
                let _ = match contents {
                    ResourceContents::Text(text) => r.str("text", text),
                    ResourceContents::Blob(blob) => r.str("blob", blob),
                };
                let resource = r.finish();
                let _ = o.str("type", "resource").raw("resource", &resource);
            }
        }
        o.finish()
    }
}

/// A successful `tools/call` answer: content blocks, optional structured
/// content, and whether the tool reports an error (`isError`).
///
/// A result with `is_error` set is a *tool execution error*: it goes back as
/// a normal result so the model can read it and correct itself, as MCP
/// prescribes. Protocol errors (an unknown tool, malformed params) are
/// JSON-RPC errors instead; see [`ToolError::protocol`].
///
/// # Examples
///
/// ```
/// use mcp_io::{ToolResult, jsonx::json};
///
/// let result = ToolResult::text("2 results").with_structured(json!({ "count": 2 }));
/// assert!(!result.is_error());
/// assert_eq!(result.content().len(), 1);
/// assert_eq!(result.structured_content().map(|s| s.to_string()), Some(r#"{"count":2}"#.to_owned()));
///
/// let failed = ToolResult::error("the date must be in the future");
/// assert!(failed.is_error());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolResult {
    content: Vec<Content>,
    structured: Option<Value<'static>>,
    is_error: bool,
}

impl ToolResult {
    /// An empty successful result.
    ///
    /// # Examples
    ///
    /// ```
    /// assert!(mcp_io::ToolResult::new().content().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A successful result with one text block.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::ToolResult::text("ok").content(), [mcp_io::Content::text("ok")]);
    /// ```
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::new().with(Content::text(text))
    }

    /// A tool execution error (`isError: true`) with one text block that
    /// tells the model what went wrong and how to fix it.
    ///
    /// # Examples
    ///
    /// ```
    /// assert!(mcp_io::ToolResult::error("no such note").is_error());
    /// ```
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        let mut result = Self::text(message);
        result.is_error = true;
        result
    }

    /// A successful result holding only structured content. A text copy of
    /// the JSON is added when the result is sent, for clients that read only
    /// `content` (MCP's backward-compatibility SHOULD).
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{ToolResult, jsonx::json};
    ///
    /// let result = ToolResult::structured(json!([1, 2]));
    /// assert!(result.content().is_empty());
    /// assert!(result.structured_content().is_some());
    /// ```
    #[must_use]
    pub fn structured(value: impl Into<Value<'static>>) -> Self {
        Self::new().with_structured(value)
    }

    /// A successful result whose structured content is `value`, any
    /// jsonx-lib [`ToJson`](crate::jsonx::ToJson) type: a struct with a
    /// hand-written impl, a map, a vector, a number. As
    /// [`structured`](Self::structured), a text copy is added when sent.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use mcp_io::ToolResult;
    ///
    /// let counts = BTreeMap::from([("open", 3u32), ("closed", 9)]);
    /// let result = ToolResult::encode(&counts);
    /// assert_eq!(result.structured_content().map(|v| v.to_string()), Some(r#"{"closed":9,"open":3}"#.to_owned()));
    /// ```
    #[must_use]
    pub fn encode<T: jsonx::ToJson + ?Sized>(value: &T) -> Self {
        let text = jsonx::to_string(value);
        // `ToJson` writes valid JSON by construction (`Written` proves one
        // value per writer); parse failure would be a jsonx-lib bug, and the
        // result then carries `null`, which an output schema refuses.
        let structured = jsonx::parse(&text).map_or(Value::Null, Value::into_owned);
        Self::structured(structured)
    }

    /// Appends a content block.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Content, ToolResult};
    ///
    /// let result = ToolResult::text("see").with(Content::resource_link("file:///a", "a"));
    /// assert_eq!(result.content().len(), 2);
    /// ```
    #[must_use]
    pub fn with(mut self, block: Content) -> Self {
        self.content.push(block);
        self
    }

    /// Sets the structured content (any JSON value; it must match the tool's
    /// output schema, if it has one).
    ///
    /// # Examples
    ///
    /// See [`ToolResult`].
    #[must_use]
    pub fn with_structured(mut self, value: impl Into<Value<'static>>) -> Self {
        self.structured = Some(value.into());
        self
    }

    /// Marks the result as a tool execution error, or not.
    ///
    /// # Examples
    ///
    /// ```
    /// assert!(mcp_io::ToolResult::text("x").with_error(true).is_error());
    /// ```
    #[must_use]
    pub fn with_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// The content blocks.
    #[must_use]
    pub fn content(&self) -> &[Content] {
        &self.content
    }

    /// The structured content, if any.
    #[must_use]
    pub fn structured_content(&self) -> Option<&Value<'static>> {
        self.structured.as_ref()
    }

    /// Whether this is a tool execution error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.is_error
    }

    /// Writes the result's members (`content`, `structuredContent`,
    /// `isError`) into `o`, adding the text copy of structured content when
    /// there is no other content.
    pub(crate) fn write_members(&self, o: &mut Obj) {
        let fallback;
        let blocks: &[Content] = match (&self.structured, self.content.is_empty()) {
            (Some(structured), true) => {
                fallback = [Content::text(jsonx::to_string(structured))];
                &fallback
            }
            _ => &self.content,
        };
        let _ = o.with("content", |out| {
            out.push('[');
            for (i, block) in blocks.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&block.to_json());
            }
            out.push(']');
        });
        if let Some(structured) = &self.structured {
            let _ = o.value("structuredContent", structured);
        }
        if self.is_error {
            let _ = o.bool("isError", true);
        }
    }
}

/// Why a tool handler did not produce a result.
///
/// Most failures are tool execution errors: [`ToolError::new`] (or `?` on any
/// `std::error::Error`) becomes a result with `isError: true` and the message
/// as its text, which the model can read and act on. Keep messages
/// actionable and free of secrets: they are sent to the client. A failure the
/// model cannot fix by retrying with other arguments can be a protocol error
/// instead ([`ToolError::protocol`], [`ToolError::missing_capability`]).
///
/// # Examples
///
/// ```
/// use mcp_io::ToolError;
///
/// fn parse(text: &str) -> Result<u32, ToolError> {
///     Ok(text.parse::<u32>()?) // ParseIntError -> a tool execution error
/// }
/// let err = parse("x").unwrap_err();
/// assert_eq!(err.to_string(), "invalid digit found in string");
/// ```
#[derive(Clone, Debug)]
pub struct ToolError {
    pub(crate) kind: ToolErrorKind,
}

#[derive(Clone, Debug)]
pub(crate) enum ToolErrorKind {
    /// Reported in a result with `isError: true`.
    Execution(String),
    /// Answered as a JSON-RPC error.
    Protocol(ErrorObject),
}

impl ToolError {
    /// A tool execution error with `message`.
    ///
    /// # Examples
    ///
    /// ```
    /// let err = mcp_io::ToolError::new("the file is locked; retry in a minute");
    /// assert_eq!(err.to_string(), "the file is locked; retry in a minute");
    /// ```
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::Execution(message.into()),
        }
    }

    /// A JSON-RPC error answer instead of a result.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{ToolError, rpc_io::ErrorObject};
    ///
    /// let err = ToolError::protocol(ErrorObject::internal_error("index unavailable"));
    /// assert_eq!(err.to_string(), "index unavailable (code -32603)");
    /// ```
    #[must_use]
    pub fn protocol(error: ErrorObject) -> Self {
        Self {
            kind: ToolErrorKind::Protocol(error),
        }
    }

    /// MCP's `MissingRequiredClientCapability` (`-32021`): the call needs a
    /// client capability the request did not declare. `required` names it as
    /// a `ClientCapabilities` object.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{ToolError, jsonx::json};
    ///
    /// let err = ToolError::missing_capability(&json!({ "sampling": {} }));
    /// assert!(err.to_string().contains("-32021"));
    /// ```
    #[must_use]
    pub fn missing_capability(required: &Value<'_>) -> Self {
        Self::protocol(crate::protocol::missing_capability(required))
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ToolErrorKind::Execution(message) => f.write_str(message),
            ToolErrorKind::Protocol(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl<E: std::error::Error> From<E> for ToolError {
    fn from(error: E) -> Self {
        Self::new(error.to_string())
    }
}

/// Standard base64 (RFC 4648, with padding).
pub(crate) fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let sextet = |shift: u32| char::from(ALPHABET[((n >> shift) & 63) as usize]);
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonx::json;

    #[test]
    fn test_base64_matches_rfc_4648_vectors() {
        for (input, output) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), output);
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    fn members(result: &ToolResult) -> String {
        let mut o = Obj::new();
        result.write_members(&mut o);
        o.finish()
    }

    #[test]
    fn test_result_members() {
        assert_eq!(members(&ToolResult::new()), r#"{"content":[]}"#);
        assert_eq!(
            members(&ToolResult::error("bad")),
            r#"{"content":[{"type":"text","text":"bad"}],"isError":true}"#
        );
        assert_eq!(
            members(&ToolResult::structured(json!({ "a": 1 }))),
            r#"{"content":[{"type":"text","text":"{\"a\":1}"}],"structuredContent":{"a":1}}"#
        );
        assert_eq!(
            members(&ToolResult::text("x").with_structured(json!(null))),
            r#"{"content":[{"type":"text","text":"x"}],"structuredContent":null}"#
        );
    }

    #[test]
    fn test_every_content_block_renders() {
        let blocks = [
            Content::audio_bytes(b"RIFF", "audio/wav"),
            Content::embedded_text("u", "t"),
            Content::embedded_bytes("u", b"x").with_mime_type("application/octet-stream"),
            Content::resource_link("u", "n")
                .with_description("d")
                .with_mime_type("m"),
            Content::image("aGk=", "image/png").with_description("ignored"),
        ];
        for block in &blocks {
            assert!(crate::jsonx::parse(&block.to_json()).is_ok());
        }
        assert_eq!(
            blocks[3].to_json(),
            r#"{"type":"resource_link","uri":"u","name":"n","mimeType":"m","description":"d"}"#
        );
    }

    #[test]
    fn test_tool_error_display_and_conversion() {
        let io = std::io::Error::other("disk full");
        let err: ToolError = io.into();
        assert_eq!(err.to_string(), "disk full");
        assert!(matches!(err.kind, ToolErrorKind::Execution(_)));
    }
}
