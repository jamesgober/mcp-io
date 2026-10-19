//! Compiling a JSON Schema document into dense nodes.
//!
//! Every subschema becomes one [`Node`] in a vector, and every reference to a
//! subschema (`properties`, `items`, `$ref`, ...) is an index into it, so
//! validation follows integers instead of looking names up. `$ref` targets
//! are resolved here, once: a reference that cannot be resolved, that
//! points outside the document, or that loops back to the same instance
//! without descending into it refuses the schema at registration rather
//! than letting it accept everything, or refuse everything, at call time.

use std::collections::BTreeMap;

use crate::jsonx::Value;

use super::{
    SchemaError, SchemaErrorKind, SchemaLimits, canon,
    decimal::{Decimal, MAX_DIVISOR_DIGITS},
};

/// The dialect a schema without `$schema` uses, and the only one accepted.
pub(crate) const DIALECT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// Keywords of 2020-12 (or of older drafts) whose assertions this validator
/// does not implement. A schema using one is refused at registration: ignoring
/// it would accept input the schema's author meant to refuse.
const UNSUPPORTED: &[&str] = &[
    "pattern",
    "patternProperties",
    "unevaluatedItems",
    "unevaluatedProperties",
    "$dynamicRef",
    "$dynamicAnchor",
    "$recursiveRef",
    "$recursiveAnchor",
    "$vocabulary",
    // Draft-07 keywords that 2020-12 replaced; under 2020-12 they would be
    // silently ignored, which is never what a schema using them meant.
    "dependencies",
    "additionalItems",
];

/// Keywords that only annotate: they never make an instance invalid.
const ANNOTATIONS: &[&str] = &[
    "$comment",
    "title",
    "description",
    "default",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
    "format",
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
];

/// The JSON types `type` can name, as bits of [`Keywords::types`].
pub(crate) mod types {
    pub(crate) const NULL: u8 = 1;
    pub(crate) const BOOLEAN: u8 = 1 << 1;
    pub(crate) const OBJECT: u8 = 1 << 2;
    pub(crate) const ARRAY: u8 = 1 << 3;
    pub(crate) const NUMBER: u8 = 1 << 4;
    pub(crate) const STRING: u8 = 1 << 5;
    pub(crate) const INTEGER: u8 = 1 << 6;

    pub(crate) fn from_name(name: &str) -> Option<u8> {
        Some(match name {
            "null" => NULL,
            "boolean" => BOOLEAN,
            "object" => OBJECT,
            "array" => ARRAY,
            "number" => NUMBER,
            "string" => STRING,
            "integer" => INTEGER,
            _ => return None,
        })
    }

    /// The names in `bits`, for error messages.
    pub(crate) fn names(bits: u8) -> String {
        let all = [
            (NULL, "null"),
            (BOOLEAN, "boolean"),
            (OBJECT, "object"),
            (ARRAY, "array"),
            (NUMBER, "number"),
            (STRING, "string"),
            (INTEGER, "integer"),
        ];
        let named: Vec<&str> = all
            .iter()
            .filter(|(bit, _)| bits & bit != 0)
            .map(|(_, name)| *name)
            .collect();
        named.join(" or ")
    }
}

/// One compiled subschema.
#[derive(Debug)]
pub(crate) enum Node {
    /// `true` (anything) or `false` (nothing).
    Bool(bool),
    /// An object schema.
    Keywords(Box<Keywords>),
}

/// The assertions and applicators of one object schema. Subschemas are node
/// indices.
#[derive(Debug, Default)]
pub(crate) struct Keywords {
    pub(crate) types: u8,
    pub(crate) enumeration: Option<Vec<String>>,
    pub(crate) constant: Option<String>,
    pub(crate) multiple_of: Option<Decimal>,
    pub(crate) minimum: Option<Decimal>,
    pub(crate) maximum: Option<Decimal>,
    pub(crate) exclusive_minimum: Option<Decimal>,
    pub(crate) exclusive_maximum: Option<Decimal>,
    pub(crate) min_length: Option<u64>,
    pub(crate) max_length: Option<u64>,
    pub(crate) min_items: Option<u64>,
    pub(crate) max_items: Option<u64>,
    pub(crate) unique_items: bool,
    pub(crate) min_contains: Option<u64>,
    pub(crate) max_contains: Option<u64>,
    pub(crate) min_properties: Option<u64>,
    pub(crate) max_properties: Option<u64>,
    pub(crate) required: Vec<Box<str>>,
    pub(crate) dependent_required: Vec<(Box<str>, Vec<Box<str>>)>,
    pub(crate) all_of: Vec<u32>,
    pub(crate) any_of: Vec<u32>,
    pub(crate) one_of: Vec<u32>,
    pub(crate) not: Option<u32>,
    pub(crate) if_: Option<u32>,
    pub(crate) then: Option<u32>,
    pub(crate) else_: Option<u32>,
    /// Sorted by name, for binary search.
    pub(crate) properties: Vec<(Box<str>, u32)>,
    pub(crate) additional_properties: Option<u32>,
    pub(crate) property_names: Option<u32>,
    pub(crate) dependent_schemas: Vec<(Box<str>, u32)>,
    pub(crate) prefix_items: Vec<u32>,
    pub(crate) items: Option<u32>,
    pub(crate) contains: Option<u32>,
    pub(crate) reference: Option<u32>,
}

/// A `$ref` waiting for its target, resolved after the walk.
struct PendingRef {
    node: usize,
    target: String,
    at: String,
}

pub(crate) struct Compiler<'d> {
    root: &'d Value<'d>,
    limits: SchemaLimits,
    nodes: Vec<Node>,
    by_pointer: BTreeMap<String, u32>,
    anchors: BTreeMap<String, String>,
    refs: Vec<PendingRef>,
}

impl<'d> Compiler<'d> {
    pub(crate) fn new(root: &'d Value<'d>, limits: SchemaLimits) -> Self {
        Self {
            root,
            limits,
            nodes: Vec::new(),
            by_pointer: BTreeMap::new(),
            anchors: BTreeMap::new(),
            refs: Vec::new(),
        }
    }

    /// Compiles the whole document; returns the nodes and the root's index.
    pub(crate) fn run(mut self) -> Result<(Vec<Node>, u32), SchemaError> {
        if let Some(object) = self.root.as_object() {
            if let Some(dialect) = object.get("$schema") {
                check_dialect(dialect, "")?;
            }
        }
        let root = self.compile(self.root, String::new(), 0)?;
        self.resolve_refs()?;
        self.refuse_cycles()?;
        Ok((self.nodes, root))
    }

    /// Refuses a subschema that applies itself to the same instance: a
    /// cycle of `$ref` (and `allOf`, `anyOf`, `oneOf`, `not`, `if`, `then`,
    /// `else`, `dependentSchemas`, which apply to the instance in hand)
    /// that never descends into a property or an item. Validation through
    /// it would never end; refusing it here keeps a server bug from
    /// reading as an over-budget client.
    fn refuse_cycles(&self) -> Result<(), SchemaError> {
        // 0: unvisited, 1: on the current path, 2: done.
        let mut state = vec![0u8; self.nodes.len()];
        let mut stack: Vec<(u32, Vec<u32>)> = Vec::new();
        for start in 0..self.nodes.len() {
            if state.get(start).copied() != Some(0) {
                continue;
            }
            let start = u32::try_from(start).unwrap_or(u32::MAX);
            if let Some(s) = state.get_mut(start as usize) {
                *s = 1;
            }
            stack.push((start, self.same_instance(start)));
            while let Some((node, next)) = stack.last_mut() {
                let node = *node;
                let Some(child) = next.pop() else {
                    if let Some(s) = state.get_mut(node as usize) {
                        *s = 2;
                    }
                    let _done = stack.pop();
                    continue;
                };
                match state.get(child as usize).copied() {
                    Some(0) => {
                        if let Some(s) = state.get_mut(child as usize) {
                            *s = 1;
                        }
                        stack.push((child, self.same_instance(child)));
                    }
                    Some(1) => {
                        let pointer = self
                            .by_pointer
                            .iter()
                            .find(|&(_, &index)| index == child)
                            .map(|(pointer, _)| pointer.clone())
                            .unwrap_or_default();
                        return Err(SchemaError::new(pointer, SchemaErrorKind::ReferenceCycle));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// The subschemas node `index` applies to the instance it is given.
    fn same_instance(&self, index: u32) -> Vec<u32> {
        let Some(Node::Keywords(k)) = self.nodes.get(index as usize) else {
            return Vec::new();
        };
        let mut out: Vec<u32> = k
            .all_of
            .iter()
            .chain(&k.any_of)
            .chain(&k.one_of)
            .copied()
            .collect();
        out.extend(
            [k.reference, k.not, k.if_, k.then, k.else_]
                .into_iter()
                .flatten(),
        );
        out.extend(k.dependent_schemas.iter().map(|(_, schema)| *schema));
        out
    }

    fn compile(
        &mut self,
        value: &Value<'_>,
        pointer: String,
        depth: usize,
    ) -> Result<u32, SchemaError> {
        if let Some(&index) = self.by_pointer.get(&pointer) {
            return Ok(index);
        }
        if depth > self.limits.max_depth {
            return Err(SchemaError::new(
                pointer,
                SchemaErrorKind::TooDeep {
                    limit: self.limits.max_depth,
                },
            ));
        }
        if self.nodes.len() >= self.limits.max_nodes {
            return Err(SchemaError::new(
                pointer,
                SchemaErrorKind::TooLarge {
                    limit: self.limits.max_nodes,
                },
            ));
        }
        let index = u32::try_from(self.nodes.len()).map_err(|_| {
            SchemaError::new(
                pointer.clone(),
                SchemaErrorKind::TooLarge {
                    limit: self.limits.max_nodes,
                },
            )
        })?;
        let _previous = self.by_pointer.insert(pointer.clone(), index);
        match value {
            Value::Bool(b) => {
                self.nodes.push(Node::Bool(*b));
            }
            Value::Object(_) => {
                // Reserve the slot first, so cycles through `$ref` and
                // children compiled below see a stable index.
                self.nodes.push(Node::Bool(true));
                let keywords = self.keywords(value, &pointer, depth, index as usize)?;
                if let Some(slot) = self.nodes.get_mut(index as usize) {
                    *slot = Node::Keywords(Box::new(keywords));
                }
            }
            other => {
                return Err(SchemaError::new(
                    pointer,
                    SchemaErrorKind::NotASchema {
                        found: other.kind().name(),
                    },
                ));
            }
        }
        Ok(index)
    }

    fn child(
        &mut self,
        value: &Value<'_>,
        pointer: &str,
        keyword: &str,
        depth: usize,
    ) -> Result<u32, SchemaError> {
        self.compile(value, push_token(pointer, keyword), depth + 1)
    }

    fn keywords(
        &mut self,
        value: &Value<'_>,
        pointer: &str,
        depth: usize,
        index: usize,
    ) -> Result<Keywords, SchemaError> {
        let mut k = Keywords::default();
        let Some(object) = value.as_object() else {
            return Ok(k);
        };
        for (key, member) in object.iter() {
            let at = push_token(pointer, key);
            let invalid = |reason: &'static str| {
                SchemaError::new(
                    at.clone(),
                    SchemaErrorKind::InvalidKeyword {
                        keyword: key.to_owned(),
                        reason,
                    },
                )
            };
            if UNSUPPORTED.contains(&key) {
                return Err(SchemaError::new(
                    at,
                    SchemaErrorKind::Unsupported {
                        keyword: key.to_owned(),
                    },
                ));
            }
            if ANNOTATIONS.contains(&key) {
                continue;
            }
            match key {
                "$schema" => check_dialect(member, &at)?,
                "$id" => {
                    if !pointer.is_empty() {
                        return Err(SchemaError::new(
                            at,
                            SchemaErrorKind::Unsupported {
                                keyword: "$id in a subschema".to_owned(),
                            },
                        ));
                    }
                    if member.as_str().is_none() {
                        return Err(invalid("must be a string"));
                    }
                }
                "$anchor" => {
                    let name = member
                        .as_str()
                        .filter(|n| is_anchor_name(n))
                        .ok_or_else(|| invalid("must be a plain-name anchor"))?;
                    if self
                        .anchors
                        .insert(name.to_owned(), pointer.to_owned())
                        .is_some()
                    {
                        return Err(SchemaError::new(
                            at,
                            SchemaErrorKind::DuplicateAnchor {
                                name: name.to_owned(),
                            },
                        ));
                    }
                }
                "$ref" => {
                    let target = member.as_str().ok_or_else(|| invalid("must be a string"))?;
                    self.refs.push(PendingRef {
                        node: index,
                        target: target.to_owned(),
                        at,
                    });
                }
                "$defs" => {
                    let defs = member
                        .as_object()
                        .ok_or_else(|| invalid("must be an object of schemas"))?;
                    for (name, def) in defs.iter() {
                        let _index = self.compile(def, push_token(&at, name), depth + 1)?;
                    }
                }
                "type" => {
                    k.types = parse_types(member).ok_or_else(|| {
                        invalid("must be a type name or a non-empty array of unique type names")
                    })?
                }
                "enum" => {
                    let items = member
                        .as_array()
                        .ok_or_else(|| invalid("must be an array"))?;
                    let mut texts = Vec::with_capacity(items.len());
                    for item in items.iter() {
                        texts.push(
                            canon::text(item)
                                .ok()
                                .ok_or_else(|| invalid("holds a value nested too deeply"))?,
                        );
                    }
                    texts.sort();
                    texts.dedup();
                    k.enumeration = Some(texts);
                }
                "const" => {
                    k.constant = Some(canon::text(member).ok().ok_or_else(|| {
                        invalid("is nested too deeply or holds a number out of range")
                    })?)
                }
                "multipleOf" => {
                    let d = number(member)
                        .filter(|d| !d.is_negative() && !d.is_zero())
                        .ok_or_else(|| invalid("must be a number greater than 0"))?;
                    if d.mantissa_digits() > MAX_DIVISOR_DIGITS {
                        return Err(SchemaError::new(
                            at,
                            SchemaErrorKind::Unsupported {
                                keyword: "multipleOf with more than 19 significant digits"
                                    .to_owned(),
                            },
                        ));
                    }
                    k.multiple_of = Some(d);
                }
                "minimum" => {
                    k.minimum = Some(number(member).ok_or_else(|| invalid("must be a number"))?)
                }
                "maximum" => {
                    k.maximum = Some(number(member).ok_or_else(|| invalid("must be a number"))?)
                }
                "exclusiveMinimum" => {
                    k.exclusive_minimum =
                        Some(number(member).ok_or_else(|| invalid("must be a number"))?)
                }
                "exclusiveMaximum" => {
                    k.exclusive_maximum =
                        Some(number(member).ok_or_else(|| invalid("must be a number"))?)
                }
                "minLength" => {
                    k.min_length = Some(
                        count(member).ok_or_else(|| invalid("must be a non-negative integer"))?,
                    )
                }
                "maxLength" => {
                    k.max_length = Some(
                        count(member).ok_or_else(|| invalid("must be a non-negative integer"))?,
                    )
                }
                "minItems" => {
                    k.min_items = Some(
                        count(member).ok_or_else(|| invalid("must be a non-negative integer"))?,
                    )
                }
                "maxItems" => {
                    k.max_items = Some(
                        count(member).ok_or_else(|| invalid("must be a non-negative integer"))?,
                    )
                }
                "minContains" => {
                    k.min_contains = Some(
                        count(member).ok_or_else(|| invalid("must be a non-negative integer"))?,
                    )
                }
                "maxContains" => {
                    k.max_contains = Some(
                        count(member).ok_or_else(|| invalid("must be a non-negative integer"))?,
                    )
                }
                "minProperties" => {
                    k.min_properties = Some(
                        count(member).ok_or_else(|| invalid("must be a non-negative integer"))?,
                    )
                }
                "maxProperties" => {
                    k.max_properties = Some(
                        count(member).ok_or_else(|| invalid("must be a non-negative integer"))?,
                    )
                }
                "uniqueItems" => {
                    k.unique_items = member
                        .as_bool()
                        .ok_or_else(|| invalid("must be a boolean"))?
                }
                "required" => {
                    k.required = names(member)
                        .ok_or_else(|| invalid("must be an array of unique strings"))?
                }
                "dependentRequired" => {
                    let deps = member
                        .as_object()
                        .ok_or_else(|| invalid("must be an object of string arrays"))?;
                    for (name, list) in deps.iter() {
                        let list = names(list).ok_or_else(|| {
                            invalid("must be an object of arrays of unique strings")
                        })?;
                        k.dependent_required.push((name.into(), list));
                    }
                }
                "allOf" | "anyOf" | "oneOf" => {
                    let items = member
                        .as_array()
                        .filter(|a| !a.is_empty())
                        .ok_or_else(|| invalid("must be a non-empty array of schemas"))?;
                    let mut indices = Vec::with_capacity(items.len());
                    for (i, item) in items.iter().enumerate() {
                        indices.push(self.compile(
                            item,
                            push_token(&at, &i.to_string()),
                            depth + 1,
                        )?);
                    }
                    match key {
                        "allOf" => k.all_of = indices,
                        "anyOf" => k.any_of = indices,
                        _ => k.one_of = indices,
                    }
                }
                "prefixItems" => {
                    let items = member
                        .as_array()
                        .filter(|a| !a.is_empty())
                        .ok_or_else(|| invalid("must be a non-empty array of schemas"))?;
                    for (i, item) in items.iter().enumerate() {
                        let index =
                            self.compile(item, push_token(&at, &i.to_string()), depth + 1)?;
                        k.prefix_items.push(index);
                    }
                }
                "items" => {
                    if member.as_array().is_some() {
                        return Err(invalid(
                            "must be a schema in 2020-12 (use prefixItems for a tuple)",
                        ));
                    }
                    k.items = Some(self.child(member, pointer, key, depth)?);
                }
                "not" => k.not = Some(self.child(member, pointer, key, depth)?),
                "if" => k.if_ = Some(self.child(member, pointer, key, depth)?),
                "then" => k.then = Some(self.child(member, pointer, key, depth)?),
                "else" => k.else_ = Some(self.child(member, pointer, key, depth)?),
                "additionalProperties" => {
                    k.additional_properties = Some(self.child(member, pointer, key, depth)?)
                }
                "propertyNames" => {
                    k.property_names = Some(self.child(member, pointer, key, depth)?)
                }
                "contains" => k.contains = Some(self.child(member, pointer, key, depth)?),
                "properties" | "dependentSchemas" => {
                    let map = member
                        .as_object()
                        .ok_or_else(|| invalid("must be an object of schemas"))?;
                    let mut entries = Vec::with_capacity(map.len());
                    for (name, schema) in map.iter() {
                        entries.push((
                            Box::<str>::from(name),
                            self.compile(schema, push_token(&at, name), depth + 1)?,
                        ));
                    }
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    if key == "properties" {
                        k.properties = entries;
                    } else {
                        k.dependent_schemas = entries;
                    }
                }
                // Anything else is an unknown keyword, which 2020-12 treats
                // as an annotation (`x-mcp-header`, vendor extensions, a
                // draft-07 `definitions` container that `$ref` can still
                // point into).
                _ => {}
            }
        }
        // `then`/`else` without `if` never apply (2020-12 §10.2.2).
        if k.if_.is_none() {
            k.then = None;
            k.else_ = None;
        }
        Ok(k)
    }

    fn resolve_refs(&mut self) -> Result<(), SchemaError> {
        // Resolving can compile new subschemas (targets nothing else
        // reached), which can hold references of their own.
        while let Some(pending) = self.refs.pop() {
            let target = self.locate(&pending)?;
            if let Some(Node::Keywords(k)) = self.nodes.get_mut(pending.node) {
                k.reference = Some(target);
            }
        }
        Ok(())
    }

    fn locate(&mut self, pending: &PendingRef) -> Result<u32, SchemaError> {
        let error = |kind| SchemaError::new(pending.at.clone(), kind);
        let fragment = match pending.target.strip_prefix('#') {
            Some(fragment) => fragment,
            None => {
                return Err(error(SchemaErrorKind::NonLocalRef {
                    reference: pending.target.clone(),
                }));
            }
        };
        let fragment = percent_decode(fragment).ok_or_else(|| {
            error(SchemaErrorKind::UnresolvedRef {
                reference: pending.target.clone(),
            })
        })?;
        let pointer = if fragment.is_empty() || fragment.starts_with('/') {
            fragment
        } else {
            match self.anchors.get(&fragment) {
                Some(pointer) => pointer.clone(),
                None => {
                    return Err(error(SchemaErrorKind::UnresolvedRef {
                        reference: pending.target.clone(),
                    }));
                }
            }
        };
        if let Some(&index) = self.by_pointer.get(&pointer) {
            return Ok(index);
        }
        let root = self.root;
        let value = root.pointer(&pointer).ok_or_else(|| {
            error(SchemaErrorKind::UnresolvedRef {
                reference: pending.target.clone(),
            })
        })?;
        self.compile(value, pointer, 1)
    }
}

fn check_dialect(value: &Value<'_>, at: &str) -> Result<(), SchemaError> {
    let uri = value.as_str().unwrap_or_default();
    let uri = uri.strip_suffix('#').unwrap_or(uri);
    if uri == DIALECT_2020_12 {
        Ok(())
    } else {
        Err(SchemaError::new(
            at.to_owned(),
            SchemaErrorKind::UnsupportedDialect {
                dialect: value.as_str().unwrap_or("(not a string)").to_owned(),
            },
        ))
    }
}

fn parse_types(value: &Value<'_>) -> Option<u8> {
    if let Some(name) = value.as_str() {
        return types::from_name(name);
    }
    let items = value.as_array().filter(|a| !a.is_empty())?;
    let mut bits = 0;
    for item in items.iter() {
        let bit = types::from_name(item.as_str()?)?;
        if bits & bit != 0 {
            return None;
        }
        bits |= bit;
    }
    Some(bits)
}

fn number(value: &Value<'_>) -> Option<Decimal> {
    value.as_number().and_then(|n| Decimal::parse(n.as_str()))
}

fn count(value: &Value<'_>) -> Option<u64> {
    number(value)?.to_u64_saturating()
}

fn names(value: &Value<'_>) -> Option<Vec<Box<str>>> {
    let items = value.as_array()?;
    let mut out: Vec<Box<str>> = Vec::with_capacity(items.len());
    for item in items.iter() {
        out.push(item.as_str()?.into());
    }
    let mut sorted: Vec<&str> = out.iter().map(AsRef::as_ref).collect();
    sorted.sort_unstable();
    if sorted.windows(2).any(|w| w[0] == w[1]) {
        return None;
    }
    Some(out)
}

/// `$anchor` names: a letter or `_`, then letters, digits, `-`, `_`, `.`.
fn is_anchor_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Appends one JSON Pointer token (RFC 6901 escaping) to `pointer`.
pub(crate) fn push_token(pointer: &str, token: &str) -> String {
    let mut out = String::with_capacity(pointer.len() + token.len() + 1);
    out.push_str(pointer);
    out.push('/');
    for c in token.chars() {
        match c {
            '~' => out.push_str("~0"),
            '/' => out.push_str("~1"),
            c => out.push(c),
        }
    }
    out
}

/// Decodes `%XX` escapes in a URI fragment. `None` for a malformed escape or
/// bytes that are not UTF-8.
fn percent_decode(fragment: &str) -> Option<String> {
    if !fragment.contains('%') {
        return Some(fragment.to_owned());
    }
    let bytes = fragment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&b) = bytes.get(at) {
        if b == b'%' {
            let hex = bytes.get(at + 1..at + 3)?;
            let text = core::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(text, 16).ok()?);
            at += 3;
        } else {
            out.push(b);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percent_decode_handles_escapes_and_rejects_malformed() {
        assert_eq!(percent_decode("/a%20b").as_deref(), Some("/a b"));
        assert_eq!(percent_decode("/plain").as_deref(), Some("/plain"));
        assert_eq!(percent_decode("/bad%2"), None);
        assert_eq!(percent_decode("/bad%zz"), None);
        assert_eq!(percent_decode("/%ff"), None);
    }

    #[test]
    fn test_push_token_escapes_per_rfc_6901() {
        assert_eq!(push_token("", "a/b~c"), "/a~1b~0c");
        assert_eq!(push_token("/x", "0"), "/x/0");
    }

    #[test]
    fn test_anchor_names() {
        assert!(is_anchor_name("node"));
        assert!(is_anchor_name("_x-1.2"));
        assert!(!is_anchor_name("1x"));
        assert!(!is_anchor_name(""));
        assert!(!is_anchor_name("a/b"));
    }

    #[test]
    fn test_type_names_render_for_messages() {
        assert_eq!(types::names(types::STRING | types::NULL), "null or string");
    }
}
