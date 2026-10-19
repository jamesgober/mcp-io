//! Tool definitions and permission tiers.

use core::fmt;

use crate::{error::RegisterError, json::Obj, jsonx::Value};

/// How much a tool can change, which decides what the permission gate asks
/// before it runs (see [`Policy`](crate::Policy)) and which annotations the
/// client is shown.
///
/// Tiers are ordered: `ReadOnly < Mutating < Destructive < Publish`.
///
/// | Tier | Annotations sent | Gate (with the default policy) |
/// |---|---|---|
/// | `ReadOnly` | `readOnlyHint: true` | runs |
/// | `Mutating` | `readOnlyHint: false`, `destructiveHint: false` | runs |
/// | `Destructive` | `readOnlyHint: false`, `destructiveHint: true` | asks the user through the client first |
/// | `Publish` | `readOnlyHint: false`, `destructiveHint: true`, `openWorldHint: true` | always asks the user through the client first; no policy can waive it |
///
/// The tier is the server author's declaration. Annotations are hints that
/// clients must treat as untrusted; the gate is what this crate enforces on
/// its own side.
///
/// # Examples
///
/// ```
/// use mcp_io::Tier;
///
/// assert!(Tier::ReadOnly < Tier::Publish);
/// assert_eq!(Tier::Destructive.name(), "destructive");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Tier {
    /// Reads and computes only: does not modify its environment (inspect,
    /// parse, check, search).
    ReadOnly,
    /// Changes its environment in ways that are additive or easy to undo
    /// (edit a draft, write to a staging area, run in a sandbox).
    Mutating,
    /// May destroy or overwrite state that cannot be recovered (delete,
    /// overwrite, force-push).
    Destructive,
    /// Acts on the world outside the host, irreversibly: publish a release,
    /// send a message, spend money. Always needs the user's confirmation.
    Publish,
}

impl Tier {
    /// The tier's lower-case name.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::Tier::ReadOnly.name(), "read-only");
    /// ```
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Mutating => "mutating",
            Self::Destructive => "destructive",
            Self::Publish => "publish",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The definition of one tool: its name, tier, descriptions, and schemas.
///
/// Build it, then register it with a handler through
/// [`Server::tool`](crate::Server::tool) or [`Tools::add`](crate::Tools::add);
/// registration checks the name and compiles the schemas.
///
/// # Examples
///
/// ```
/// use mcp_io::{Tier, Tool, jsonx::json};
///
/// let tool = Tool::new("search", Tier::ReadOnly)
///     .title("Search the notes")
///     .description("Full-text search over the notes; returns matching titles.")
///     .input_schema(json!({
///         "type": "object",
///         "properties": { "query": { "type": "string", "minLength": 1 } },
///         "required": ["query"],
///         "additionalProperties": false
///     }))
///     .idempotent(true)
///     .open_world(false);
/// assert_eq!(tool.name(), "search");
/// assert_eq!(tool.tier(), Tier::ReadOnly);
/// ```
#[derive(Clone, Debug)]
pub struct Tool {
    pub(crate) name: String,
    pub(crate) tier: Tier,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) input_schema: Option<Value<'static>>,
    pub(crate) output_schema: Option<Value<'static>>,
    pub(crate) idempotent: Option<bool>,
    pub(crate) open_world: Option<bool>,
    pub(crate) confirmation: Option<String>,
}

impl Tool {
    /// A tool named `name` in `tier`, taking no arguments until
    /// [`input_schema`](Self::input_schema) says otherwise.
    ///
    /// Names must be 1 to 128 characters of ASCII letters, digits, `_`, `-`
    /// and `.` (what MCP recommends); others are refused at registration.
    ///
    /// # Examples
    ///
    /// ```
    /// let tool = mcp_io::Tool::new("now", mcp_io::Tier::ReadOnly);
    /// assert!(tool.input_schema_value().is_none()); // takes `{}` only
    /// ```
    #[must_use]
    pub fn new(name: impl Into<String>, tier: Tier) -> Self {
        Self {
            name: name.into(),
            tier,
            title: None,
            description: None,
            input_schema: None,
            output_schema: None,
            idempotent: None,
            open_world: None,
            confirmation: None,
        }
    }

    /// A human-readable name for display.
    ///
    /// # Examples
    ///
    /// ```
    /// let tool = mcp_io::Tool::new("now", mcp_io::Tier::ReadOnly).title("Current time");
    /// assert_eq!(tool.title_text(), Some("Current time"));
    /// ```
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// What the tool does, for the model: when to use it, what it returns.
    ///
    /// # Examples
    ///
    /// ```
    /// let tool = mcp_io::Tool::new("now", mcp_io::Tier::ReadOnly).description("The server's clock, as RFC 3339");
    /// assert!(tool.description_text().is_some());
    /// ```
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The JSON Schema (2020-12, the supported subset: see
    /// [`schema`](crate::schema)) that every call's `arguments` must match
    /// before the handler runs. MCP requires the root to be
    /// `"type": "object"`. Without one, the tool takes no arguments:
    /// `{"type": "object", "additionalProperties": false}`.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Tier, Tool, jsonx::json};
    ///
    /// let tool = Tool::new("echo", Tier::ReadOnly).input_schema(json!({ "type": "object" }));
    /// assert!(tool.input_schema_value().is_some());
    /// ```
    #[must_use]
    pub fn input_schema(mut self, schema: impl Into<Value<'static>>) -> Self {
        self.input_schema = Some(schema.into());
        self
    }

    /// The JSON Schema of the tool's `structuredContent`. With one, every
    /// successful result must carry structured content that matches it
    /// (MCP's "servers MUST provide structured results that conform"); a
    /// handler result that does not is answered as an internal error.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Tier, Tool, jsonx::json};
    ///
    /// let tool = Tool::new("count", Tier::ReadOnly).output_schema(json!({ "type": "integer" }));
    /// assert!(tool.output_schema_value().is_some());
    /// ```
    #[must_use]
    pub fn output_schema(mut self, schema: impl Into<Value<'static>>) -> Self {
        self.output_schema = Some(schema.into());
        self
    }

    /// Sets the input schema to `T`'s ([`JsonSchema`]), so a typed tool's
    /// schema and decoder are defined in one place.
    ///
    /// # Examples
    ///
    /// See [`Server::tool_json`](crate::Server::tool_json).
    #[must_use]
    pub fn input_schema_for<T: JsonSchema + ?Sized>(self) -> Self {
        self.input_schema(T::json_schema())
    }

    /// Sets the output schema to `T`'s ([`JsonSchema`]).
    ///
    /// # Examples
    ///
    /// See [`Server::tool_json`](crate::Server::tool_json).
    #[must_use]
    pub fn output_schema_for<T: JsonSchema + ?Sized>(self) -> Self {
        self.output_schema(T::json_schema())
    }

    /// The `idempotentHint` annotation: repeating a call with the same
    /// arguments has no further effect. Sent for tiers above `ReadOnly`.
    ///
    /// # Examples
    ///
    /// ```
    /// let tool = mcp_io::Tool::new("set", mcp_io::Tier::Mutating).idempotent(true);
    /// assert_eq!(tool.annotations_json(), r#"{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true}"#);
    /// ```
    #[must_use]
    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = Some(idempotent);
        self
    }

    /// The `openWorldHint` annotation: whether the tool reaches entities
    /// outside a closed domain (the web, other people). `Publish` tools
    /// always send `true`.
    ///
    /// # Examples
    ///
    /// ```
    /// let tool = mcp_io::Tool::new("recall", mcp_io::Tier::ReadOnly).open_world(false);
    /// assert_eq!(tool.annotations_json(), r#"{"readOnlyHint":true,"openWorldHint":false}"#);
    /// ```
    #[must_use]
    pub fn open_world(mut self, open_world: bool) -> Self {
        self.open_world = Some(open_world);
        self
    }

    /// The question the user is asked before a call that needs confirmation
    /// (`Destructive` with the default policy, `Publish` always). The tool's
    /// name and the call's arguments are appended. Default: "Allow the tool
    /// to run?".
    ///
    /// # Examples
    ///
    /// ```
    /// let tool = mcp_io::Tool::new("release", mcp_io::Tier::Publish)
    ///     .confirmation("Publish this release to crates.io?");
    /// assert_eq!(tool.tier(), mcp_io::Tier::Publish);
    /// ```
    #[must_use]
    pub fn confirmation(mut self, question: impl Into<String>) -> Self {
        self.confirmation = Some(question.into());
        self
    }

    /// The name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The tier.
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// The title, if set.
    #[must_use]
    pub fn title_text(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The description, if set.
    #[must_use]
    pub fn description_text(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The input schema, if set.
    #[must_use]
    pub fn input_schema_value(&self) -> Option<&Value<'static>> {
        self.input_schema.as_ref()
    }

    /// The output schema, if set.
    #[must_use]
    pub fn output_schema_value(&self) -> Option<&Value<'static>> {
        self.output_schema.as_ref()
    }

    /// The `annotations` object sent in `tools/list`, as JSON text.
    ///
    /// # Examples
    ///
    /// ```
    /// let tool = mcp_io::Tool::new("ship", mcp_io::Tier::Publish);
    /// assert_eq!(
    ///     tool.annotations_json(),
    ///     r#"{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}"#,
    /// );
    /// ```
    #[must_use]
    pub fn annotations_json(&self) -> String {
        let mut o = Obj::new();
        match self.tier {
            Tier::ReadOnly => {
                let _ = o.bool("readOnlyHint", true);
            }
            Tier::Mutating | Tier::Destructive | Tier::Publish => {
                let _ = o
                    .bool("readOnlyHint", false)
                    .bool("destructiveHint", self.tier >= Tier::Destructive);
                if let Some(idempotent) = self.idempotent {
                    let _ = o.bool("idempotentHint", idempotent);
                }
            }
        }
        let open_world = if self.tier == Tier::Publish {
            Some(true)
        } else {
            self.open_world
        };
        if let Some(open_world) = open_world {
            let _ = o.bool("openWorldHint", open_world);
        }
        o.finish()
    }

    /// Checks the name and the root of the input schema.
    pub(crate) fn check(&self) -> Result<(), RegisterError> {
        check_name(&self.name)?;
        if let Some(schema) = &self.input_schema {
            let is_object_schema = schema
                .as_object()
                .and_then(|o| o.get("type"))
                .is_some_and(|t| t == "object");
            if !is_object_schema {
                return Err(RegisterError::InputSchemaNotObject {
                    tool: self.name.clone(),
                });
            }
        }
        Ok(())
    }

    /// The tool's definition as MCP's `Tool` object, in JSON text.
    pub(crate) fn definition_json(&self) -> String {
        let mut o = Obj::with_capacity(256);
        let _ = o
            .str("name", &self.name)
            .opt_str("title", self.title.as_deref())
            .opt_str("description", self.description.as_deref());
        match &self.input_schema {
            Some(schema) => {
                let _ = o.value("inputSchema", schema);
            }
            None => {
                let _ = o.raw("inputSchema", NO_ARGUMENTS);
            }
        }
        if let Some(schema) = &self.output_schema {
            let _ = o.value("outputSchema", schema);
        }
        let annotations = self.annotations_json();
        let _ = o.raw("annotations", &annotations);
        o.finish()
    }
}

/// A type's JSON Schema, written next to its `FromJson` or `ToJson` impl so
/// one definition describes both what the tool accepts and what it decodes
/// ([`Tool::input_schema_for`], [`Tool::output_schema_for`]). The schema is
/// still what validates a call before its handler runs; the decoder then
/// reads the validated arguments.
///
/// # Examples
///
/// ```
/// use mcp_io::{JsonSchema, Tier, Tool, jsonx::{json, Value}};
///
/// struct Query;
///
/// impl JsonSchema for Query {
///     fn json_schema() -> Value<'static> {
///         json!({ "type": "object", "properties": { "q": { "type": "string" } }, "required": ["q"] })
///     }
/// }
///
/// let tool = Tool::new("find", Tier::ReadOnly).input_schema_for::<Query>();
/// assert_eq!(tool.input_schema_value(), Some(&Query::json_schema()));
/// ```
pub trait JsonSchema {
    /// The schema (JSON Schema 2020-12, within the supported subset: see
    /// [`schema`](crate::schema)).
    fn json_schema() -> Value<'static>;
}

/// The input schema of a tool that takes no arguments, as MCP recommends.
pub(crate) const NO_ARGUMENTS: &str = r#"{"type":"object","additionalProperties":false}"#;

/// MCP's tool-name rule: 1 to 128 of `[A-Za-z0-9_.-]`.
pub(crate) fn check_name(name: &str) -> Result<(), RegisterError> {
    let reason = if name.is_empty() {
        Some("is empty")
    } else if name.len() > 128 {
        Some("is longer than 128 characters")
    } else if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        Some("may only hold ASCII letters, digits, `_`, `-` and `.`")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(RegisterError::InvalidName {
            name: crate::protocol::clip(name, 160).to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonx::json;

    #[test]
    fn test_annotations_follow_the_tier() {
        let a = |tier| Tool::new("t", tier).annotations_json();
        assert_eq!(a(Tier::ReadOnly), r#"{"readOnlyHint":true}"#);
        assert_eq!(
            a(Tier::Mutating),
            r#"{"readOnlyHint":false,"destructiveHint":false}"#
        );
        assert_eq!(
            a(Tier::Destructive),
            r#"{"readOnlyHint":false,"destructiveHint":true}"#
        );
        assert_eq!(
            Tool::new("t", Tier::Publish)
                .open_world(false)
                .annotations_json(),
            r#"{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}"#,
            "publish tools are always open-world"
        );
        assert_eq!(
            Tool::new("t", Tier::ReadOnly)
                .idempotent(true)
                .annotations_json(),
            r#"{"readOnlyHint":true}"#,
            "idempotence is meaningless for read-only tools"
        );
    }

    #[test]
    fn test_names_follow_the_mcp_rule() {
        for ok in [
            "a",
            "getUser",
            "DATA_EXPORT_v2",
            "admin.tools.list",
            &"x".repeat(128),
        ] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "has space", "comma,", "ü", &"x".repeat(129)] {
            assert!(
                matches!(check_name(bad), Err(RegisterError::InvalidName { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn test_input_schema_root_must_be_an_object_schema() {
        for schema in [
            json!({}),
            json!({ "type": "string" }),
            json!(true),
            json!({ "type": ["object"] }),
        ] {
            let tool = Tool::new("t", Tier::ReadOnly).input_schema(schema);
            assert!(matches!(
                tool.check(),
                Err(RegisterError::InputSchemaNotObject { .. })
            ));
        }
        assert!(Tool::new("t", Tier::ReadOnly).check().is_ok());
    }

    #[test]
    fn test_accessors_return_what_was_built() {
        let tool = Tool::new("t", Tier::Mutating)
            .title("T")
            .description("D")
            .input_schema(json!({ "type": "object" }))
            .output_schema(json!(true));
        assert_eq!((tool.name(), tool.tier()), ("t", Tier::Mutating));
        assert_eq!(
            (tool.title_text(), tool.description_text()),
            (Some("T"), Some("D"))
        );
        assert!(tool.input_schema_value().is_some() && tool.output_schema_value().is_some());
    }

    #[test]
    fn test_definition_json() {
        let tool = Tool::new("t", Tier::ReadOnly)
            .title("T")
            .description("d")
            .output_schema(json!({ "type": "integer" }));
        assert_eq!(
            tool.definition_json(),
            r#"{"name":"t","title":"T","description":"d","inputSchema":{"type":"object","additionalProperties":false},"outputSchema":{"type":"integer"},"annotations":{"readOnlyHint":true}}"#
        );
        assert_eq!(Tier::Publish.to_string(), "publish");
    }
}
