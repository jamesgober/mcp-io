//! JSON Schema 2020-12 validation of tool input and output, for a documented
//! subset of the vocabulary.
//!
//! MCP requires a server to "validate all tool inputs" and to return
//! structured output that conforms to the tool's `outputSchema`, with
//! JSON Schema 2020-12 as the default dialect. [`Schema`] is that validator:
//! compiled once at registration, applied to every call before the handler
//! runs.
//!
//! # What is supported
//!
//! | Group | Keywords |
//! |---|---|
//! | Any type | `type`, `enum`, `const` |
//! | Numbers | `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `multipleOf` (exact decimal arithmetic) |
//! | Strings | `minLength`, `maxLength` (counted in Unicode code points) |
//! | Arrays | `prefixItems`, `items`, `contains`, `minContains`, `maxContains`, `minItems`, `maxItems`, `uniqueItems` |
//! | Objects | `properties`, `additionalProperties`, `required`, `propertyNames`, `minProperties`, `maxProperties`, `dependentRequired`, `dependentSchemas` |
//! | Composition | `allOf`, `anyOf`, `oneOf`, `not`, `if` / `then` / `else` |
//! | Structure | `$ref` to `#`, `#/json/pointer` (including into `$defs` or any other member) and `#anchor` (`$anchor`), `$defs`, `$schema` (2020-12 only), `$id` at the root |
//! | Annotations (never fail) | `title`, `description`, `default`, `examples`, `deprecated`, `readOnly`, `writeOnly`, `$comment`, `format`, `contentEncoding`, `contentMediaType`, `contentSchema`, and unknown keywords such as `x-mcp-header` |
//!
//! Numbers compare by exact decimal value: `1` equals `1.0`, `0.3` is a
//! multiple of `0.1`, and integers beyond 2^53 stay distinct. A number whose
//! exponent is beyond ±2^52 cannot be compared exactly: an instance holding
//! one is refused (keyword `number`) wherever its value matters. `format` is an
//! annotation, as 2020-12's default vocabulary makes it: it is not asserted.
//!
//! # What is refused
//!
//! A schema that uses any of these is refused when it is compiled (a tool
//! with one cannot be registered), rather than silently accepting input its
//! author meant to refuse:
//!
//! - `pattern` and `patternProperties` (they need a regular-expression engine;
//!   there is no first-party one yet),
//! - `unevaluatedProperties` and `unevaluatedItems`,
//! - `$dynamicRef`, `$dynamicAnchor`, `$vocabulary`, and draft 2019-09's
//!   `$recursiveRef` / `$recursiveAnchor`,
//! - draft-07's `dependencies` and `additionalItems`, and `items` as an array,
//! - a `$schema` other than 2020-12, and `$id` inside a subschema,
//! - a `$ref` that is not a fragment of this document. Network references
//!   are never fetched, as MCP requires; unresolvable references refuse the
//!   schema instead of passing everything.
//!
//! # Budgets
//!
//! Compilation is bounded by [`SchemaLimits::max_depth`] and
//! [`SchemaLimits::max_nodes`]; validation by [`SchemaLimits::max_steps`]
//! (subschema applications) and [`SchemaLimits::max_validation_depth`], so
//! composition keywords and reference cycles cannot turn one call into
//! unbounded work. `uniqueItems` sorts canonical forms, `O(n log n)`.
//!
//! # Examples
//!
//! ```
//! use mcp_io::{Schema, jsonx::json};
//!
//! let schema = Schema::compile(&json!({
//!     "type": "object",
//!     "properties": { "count": { "type": "integer", "minimum": 1 } },
//!     "required": ["count"],
//!     "additionalProperties": false
//! }))?;
//! assert!(schema.validate(&json!({ "count": 3 })).is_ok());
//!
//! let errors = schema.validate(&json!({ "count": 0, "extra": true })).unwrap_err();
//! assert_eq!(errors.errors()[0].instance_path(), "/count");
//! assert_eq!(errors.errors()[0].keyword(), "minimum");
//! assert_eq!(errors.errors()[1].keyword(), "additionalProperties");
//! # Ok::<(), mcp_io::SchemaError>(())
//! ```

mod canon;
mod compile;
mod decimal;
mod validate;

use core::fmt;

use crate::jsonx::Value;

pub(crate) use canon::text as canonical_text;

/// Budgets for compiling and applying a [`Schema`].
///
/// The defaults are generous for any schema written by hand and for any
/// input within rpc-io's message budgets, and small enough that a hostile
/// schema or input cannot make one validation expensive.
///
/// # Examples
///
/// ```
/// use mcp_io::{Schema, SchemaLimits, jsonx::json};
///
/// let tight = SchemaLimits::new().with_max_steps(10);
/// let schema = Schema::compile_with(&json!({ "type": "array", "items": { "type": "integer" } }), tight)?;
/// let big: Vec<i64> = (0..100).collect();
/// let errors = schema.validate(&big.into()).unwrap_err();
/// assert!(errors.budget_exceeded());
/// # Ok::<(), mcp_io::SchemaError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SchemaLimits {
    /// How deeply subschemas may nest in the schema document. Default 64.
    pub max_depth: usize,
    /// How many subschemas one schema may hold. Default 4,096.
    pub max_nodes: usize,
    /// How many subschema applications one validation may make. Default
    /// 4,194,304 (one per value jsonx-lib's default budget lets a message
    /// hold).
    pub max_steps: u64,
    /// How deeply subschema applications may nest during one validation:
    /// each level of the instance costs one, and each `$ref` followed one
    /// more, so a recursive schema over JSON 128 levels deep (rpc-io's MCP
    /// budget) needs about 256. Default 512, which a 1 MiB stack holds even
    /// in a debug build.
    pub max_validation_depth: usize,
    /// How many errors one failed validation reports. Default 16.
    pub max_errors: usize,
}

impl SchemaLimits {
    /// The default budgets.
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::SchemaLimits::new().max_nodes, 4096);
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_depth: 64,
            max_nodes: 4096,
            max_steps: 4 * 1024 * 1024,
            max_validation_depth: 512,
            max_errors: 16,
        }
    }

    /// Sets [`max_depth`](Self::max_depth).
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::SchemaLimits::new().with_max_depth(8).max_depth, 8);
    /// ```
    #[must_use]
    pub const fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    /// Sets [`max_nodes`](Self::max_nodes).
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::SchemaLimits::new().with_max_nodes(32).max_nodes, 32);
    /// ```
    #[must_use]
    pub const fn with_max_nodes(mut self, nodes: usize) -> Self {
        self.max_nodes = nodes;
        self
    }

    /// Sets [`max_steps`](Self::max_steps).
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::SchemaLimits::new().with_max_steps(1000).max_steps, 1000);
    /// ```
    #[must_use]
    pub const fn with_max_steps(mut self, steps: u64) -> Self {
        self.max_steps = steps;
        self
    }

    /// Sets [`max_validation_depth`](Self::max_validation_depth).
    ///
    /// # Examples
    ///
    /// ```
    /// let limits = mcp_io::SchemaLimits::new().with_max_validation_depth(16);
    /// assert_eq!(limits.max_validation_depth, 16);
    /// ```
    #[must_use]
    pub const fn with_max_validation_depth(mut self, depth: usize) -> Self {
        self.max_validation_depth = depth;
        self
    }

    /// Sets [`max_errors`](Self::max_errors) (at least 1).
    ///
    /// # Examples
    ///
    /// ```
    /// assert_eq!(mcp_io::SchemaLimits::new().with_max_errors(0).max_errors, 1);
    /// ```
    #[must_use]
    pub const fn with_max_errors(mut self, errors: usize) -> Self {
        self.max_errors = if errors == 0 { 1 } else { errors };
        self
    }
}

impl Default for SchemaLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// A compiled JSON Schema: validates instances against the supported
/// 2020-12 subset (see the [module documentation](crate::schema)).
///
/// `Send + Sync`; compile once, validate from any thread.
///
/// # Examples
///
/// ```
/// use mcp_io::{Schema, jsonx::json};
///
/// let schema = Schema::compile(&json!({ "type": "string", "maxLength": 3 }))?;
/// assert!(schema.is_valid(&json!("abc")));
/// assert!(!schema.is_valid(&json!("abcd")));
/// # Ok::<(), mcp_io::SchemaError>(())
/// ```
#[derive(Debug)]
pub struct Schema {
    nodes: Vec<compile::Node>,
    root: u32,
    limits: SchemaLimits,
}

impl Schema {
    /// Compiles `schema` with the default [`SchemaLimits`].
    ///
    /// # Errors
    ///
    /// A [`SchemaError`] naming the subschema (as a JSON Pointer into
    /// `schema`) and why: not a schema, a keyword with an invalid value, a
    /// keyword or dialect this validator does not support, a `$ref` that
    /// cannot be resolved inside the document, or a budget exceeded.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Schema, SchemaErrorKind, jsonx::json};
    ///
    /// let err = Schema::compile(&json!({ "properties": { "id": { "pattern": "^[a-z]+$" } } })).unwrap_err();
    /// assert_eq!(err.pointer(), "/properties/id/pattern");
    /// assert!(matches!(err.kind(), SchemaErrorKind::Unsupported { .. }));
    /// ```
    pub fn compile(schema: &Value<'_>) -> Result<Self, SchemaError> {
        Self::compile_with(schema, SchemaLimits::new())
    }

    /// Compiles `schema` with `limits`, which also govern every later
    /// [`validate`](Self::validate).
    ///
    /// # Errors
    ///
    /// As [`compile`](Self::compile).
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Schema, SchemaErrorKind, SchemaLimits, jsonx::json};
    ///
    /// let nested = json!({ "items": { "items": { "items": {} } } });
    /// let err = Schema::compile_with(&nested, SchemaLimits::new().with_max_depth(2)).unwrap_err();
    /// assert!(matches!(err.kind(), SchemaErrorKind::TooDeep { limit: 2 }));
    /// ```
    pub fn compile_with(schema: &Value<'_>, limits: SchemaLimits) -> Result<Self, SchemaError> {
        let (nodes, root) = compile::Compiler::new(schema, limits).run()?;
        Ok(Self {
            nodes,
            root,
            limits,
        })
    }

    /// Validates `instance`.
    ///
    /// # Errors
    ///
    /// [`Violations`]: up to [`SchemaLimits::max_errors`] errors, each with
    /// the instance location (a JSON Pointer), the keyword that failed, and
    /// a message that does not quote the instance's values (property names
    /// appear, clipped to 64 bytes and marked `…`; a path is clipped to 512
    /// bytes). When the
    /// validation budget runs out, a single error with
    /// [`Violations::budget_exceeded`] set.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Schema, jsonx::json};
    ///
    /// let schema = Schema::compile(&json!({ "type": "array", "uniqueItems": true }))?;
    /// assert!(schema.validate(&json!([1, "1"])).is_ok());
    /// let errors = schema.validate(&json!([1, 1.0])).unwrap_err();
    /// assert_eq!(errors.errors()[0].keyword(), "uniqueItems");
    /// # Ok::<(), mcp_io::SchemaError>(())
    /// ```
    pub fn validate(&self, instance: &Value<'_>) -> Result<(), Violations> {
        validate::Run::new(&self.nodes, &self.limits).finish(self.root, instance)
    }

    /// Whether `instance` is valid; [`validate`](Self::validate) without the
    /// report.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Schema, jsonx::json};
    ///
    /// let schema = Schema::compile(&json!({ "enum": [1, "two", null] }))?;
    /// assert!(schema.is_valid(&json!(1.0)));
    /// assert!(!schema.is_valid(&json!("three")));
    /// # Ok::<(), mcp_io::SchemaError>(())
    /// ```
    #[must_use]
    pub fn is_valid(&self, instance: &Value<'_>) -> bool {
        self.validate(instance).is_ok()
    }

    /// The limits this schema was compiled with.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_io::{Schema, SchemaLimits, jsonx::json};
    ///
    /// let schema = Schema::compile(&json!(true))?;
    /// assert_eq!(schema.limits(), SchemaLimits::new());
    /// # Ok::<(), mcp_io::SchemaError>(())
    /// ```
    #[must_use]
    pub fn limits(&self) -> SchemaLimits {
        self.limits
    }
}

/// Why a schema could not be compiled.
///
/// # Examples
///
/// ```
/// use mcp_io::{Schema, SchemaErrorKind, jsonx::json};
///
/// let err = Schema::compile(&json!({ "$ref": "https://example.com/s.json" })).unwrap_err();
/// assert!(matches!(err.kind(), SchemaErrorKind::NonLocalRef { .. }));
/// assert_eq!(err.pointer(), "/$ref");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaError {
    pointer: String,
    kind: SchemaErrorKind,
}

impl SchemaError {
    pub(crate) fn new(pointer: String, kind: SchemaErrorKind) -> Self {
        Self { pointer, kind }
    }

    /// Where in the schema document, as a JSON Pointer (`""` is the root).
    #[must_use]
    pub fn pointer(&self) -> &str {
        &self.pointer
    }

    /// What is wrong.
    #[must_use]
    pub fn kind(&self) -> &SchemaErrorKind {
        &self.kind
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = if self.pointer.is_empty() {
            "/"
        } else {
            &self.pointer
        };
        write!(f, "invalid schema at {at}: {}", self.kind)
    }
}

impl std::error::Error for SchemaError {}

/// What made a schema impossible to compile. Each variant says what to change.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemaErrorKind {
    /// A subschema that is neither an object nor a boolean. Fix the schema.
    NotASchema {
        /// The JSON kind found instead.
        found: &'static str,
    },
    /// A keyword whose value is not what the keyword takes (`"minimum": "1"`).
    InvalidKeyword {
        /// The keyword.
        keyword: String,
        /// What it must be.
        reason: &'static str,
    },
    /// A keyword this validator does not implement. Rewrite the schema
    /// without it (for `pattern`: `enum`, or validate in the handler).
    Unsupported {
        /// The keyword, or a description of the construct.
        keyword: String,
    },
    /// A `$schema` other than 2020-12. Remove it, or set it to
    /// `https://json-schema.org/draft/2020-12/schema`.
    UnsupportedDialect {
        /// The `$schema` value.
        dialect: String,
    },
    /// The same `$anchor` twice. Rename one.
    DuplicateAnchor {
        /// The anchor.
        name: String,
    },
    /// A `$ref` to another document. References are never fetched; inline
    /// the target under `$defs` and refer to it with `#/$defs/...`.
    NonLocalRef {
        /// The reference.
        reference: String,
    },
    /// A `$ref` whose target is not in this document.
    UnresolvedRef {
        /// The reference.
        reference: String,
    },
    /// Subschemas nested deeper than [`SchemaLimits::max_depth`].
    TooDeep {
        /// The limit.
        limit: usize,
    },
    /// More subschemas than [`SchemaLimits::max_nodes`].
    TooLarge {
        /// The limit.
        limit: usize,
    },
}

impl fmt::Display for SchemaErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotASchema { found } => {
                write!(f, "a schema must be an object or a boolean, found {found}")
            }
            Self::InvalidKeyword { keyword, reason } => write!(f, "`{keyword}` {reason}"),
            Self::Unsupported { keyword } => {
                write!(f, "`{keyword}` is not supported by this validator")
            }
            Self::UnsupportedDialect { dialect } => {
                write!(
                    f,
                    "unsupported dialect {dialect:?}: only JSON Schema 2020-12 is supported"
                )
            }
            Self::DuplicateAnchor { name } => write!(f, "the anchor {name:?} is defined twice"),
            Self::NonLocalRef { reference } => {
                write!(
                    f,
                    "the reference {reference:?} points outside this schema, and references are never fetched"
                )
            }
            Self::UnresolvedRef { reference } => write!(
                f,
                "the reference {reference:?} has no target in this schema"
            ),
            Self::TooDeep { limit } => {
                write!(f, "subschemas nest deeper than the limit of {limit}")
            }
            Self::TooLarge { limit } => write!(f, "the schema has more than {limit} subschemas"),
        }
    }
}

/// One way an instance failed its schema.
///
/// # Examples
///
/// ```
/// use mcp_io::{Schema, jsonx::json};
///
/// let schema = Schema::compile(&json!({ "items": { "type": "string" } }))?;
/// let errors = schema.validate(&json!(["a", 2])).unwrap_err();
/// let error = &errors.errors()[0];
/// assert_eq!((error.instance_path(), error.keyword()), ("/1", "type"));
/// assert_eq!(error.message(), "expected string, found number");
/// assert_eq!(error.to_string(), "/1: expected string, found number");
/// # Ok::<(), mcp_io::SchemaError>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    instance_path: String,
    keyword: &'static str,
    message: String,
}

impl ValidationError {
    /// Where in the instance, as a JSON Pointer (`""` is the whole instance).
    /// Each property name in it is clipped to 64 bytes (marked `…`), and the
    /// whole path to 512 bytes, so a long key from the client is not echoed.
    #[must_use]
    pub fn instance_path(&self) -> &str {
        &self.instance_path
    }

    /// The keyword that failed (`"budget"` when the budget ran out).
    #[must_use]
    pub fn keyword(&self) -> &'static str {
        self.keyword
    }

    /// What is wrong, without quoting the instance's values (which may be
    /// sensitive). Property names are quoted (the schema's, and for
    /// `propertyNames` the instance's, clipped to 64 bytes and marked `…`).
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = if self.instance_path.is_empty() {
            "(root)"
        } else {
            &self.instance_path
        };
        write!(f, "{at}: {}", self.message)
    }
}

/// Every reported way an instance failed its schema.
///
/// # Examples
///
/// ```
/// use mcp_io::{Schema, jsonx::json};
///
/// let schema = Schema::compile(&json!({ "required": ["a", "b"] }))?;
/// let errors = schema.validate(&json!({})).unwrap_err();
/// assert_eq!(errors.errors().len(), 2);
/// assert!(!errors.truncated() && !errors.budget_exceeded());
/// assert_eq!(
///     errors.to_string(),
///     "(root): is missing the required property \"a\"; (root): is missing the required property \"b\"",
/// );
/// # Ok::<(), mcp_io::SchemaError>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violations {
    errors: Vec<ValidationError>,
    truncated: bool,
    budget_exceeded: bool,
}

impl Violations {
    /// The errors, in the order found (never empty).
    #[must_use]
    pub fn errors(&self) -> &[ValidationError] {
        &self.errors
    }

    /// Whether more errors were found than [`SchemaLimits::max_errors`].
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Whether validation stopped because the budget ran out; the instance
    /// was refused without being checked in full.
    #[must_use]
    pub fn budget_exceeded(&self) -> bool {
        self.budget_exceeded
    }
}

impl fmt::Display for Violations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, error) in self.errors.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            fmt::Display::fmt(error, f)?;
        }
        if self.truncated {
            f.write_str("; (more errors not shown)")?;
        }
        Ok(())
    }
}

impl std::error::Error for Violations {}

#[cfg(test)]
mod tests;
