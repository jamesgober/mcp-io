//! Validating an instance against compiled nodes.
//!
//! One walk applies every keyword of every subschema the instance reaches.
//! The walk is budgeted: each subschema application is a step, and the depth
//! of nested applications (instance depth plus `$ref` chains) is capped, so a
//! hostile instance or a self-referencing schema ends in a refusal, never in
//! unbounded work or a stack overflow. Errors are recorded only on the
//! path that decides the result: branches of `anyOf`, `oneOf`, `not`, `if`
//! and `contains` are probes, and their failures are not reported one by one.

use std::borrow::Cow;

use crate::jsonx::Value;

use super::{
    SchemaLimits, ValidationError, Violations, canon,
    compile::{Keywords, Node, types},
    decimal::Decimal,
};

/// A numeric bound: its value, its keyword, whether an instance satisfies
/// it, and how the message phrases it.
type Bound<'k> = (
    &'k Option<Decimal>,
    &'static str,
    fn(&Decimal, &Decimal) -> bool,
    &'static str,
);

pub(crate) struct Run<'s> {
    nodes: &'s [Node],
    limits: &'s SchemaLimits,
    steps: u64,
    /// The instance path as JSON Pointer text, grown and cut as the walk
    /// descends and returns.
    path: String,
    errors: Vec<ValidationError>,
    truncated: bool,
    exhausted: bool,
    /// The instance holds a number the validator cannot compare exactly.
    out_of_range: bool,
}

impl<'s> Run<'s> {
    pub(crate) fn new(nodes: &'s [Node], limits: &'s SchemaLimits) -> Self {
        Self {
            nodes,
            limits,
            steps: 0,
            path: String::new(),
            errors: Vec::new(),
            truncated: false,
            exhausted: false,
            out_of_range: false,
        }
    }

    /// Validates `instance` against node `root`.
    pub(crate) fn finish(mut self, root: u32, instance: &Value<'_>) -> Result<(), Violations> {
        let valid = self.apply(root, instance, 0, true);
        if self.out_of_range && !self.exhausted {
            return Err(Violations {
                errors: vec![ValidationError {
                    instance_path: String::new(),
                    keyword: "number",
                    message: "holds a number whose exponent is beyond what this validator \
                              compares exactly (more than 2^52 in magnitude)"
                        .to_owned(),
                }],
                truncated: false,
                budget_exceeded: false,
            });
        }
        if self.exhausted {
            return Err(Violations {
                errors: vec![ValidationError {
                    instance_path: String::new(),
                    keyword: "budget",
                    message: "the input is too large or too deeply nested to validate within the server's budget".to_owned(),
                }],
                truncated: false,
                budget_exceeded: true,
            });
        }
        if valid {
            Ok(())
        } else {
            if self.errors.is_empty() {
                self.errors.push(ValidationError {
                    instance_path: String::new(),
                    keyword: "schema",
                    message: "does not match the schema".to_owned(),
                });
            }
            Err(Violations {
                errors: self.errors,
                truncated: self.truncated,
                budget_exceeded: false,
            })
        }
    }

    fn record(&mut self, record: bool, keyword: &'static str, message: impl Into<String>) {
        if !record {
            return;
        }
        if self.errors.len() >= self.limits.max_errors {
            self.truncated = true;
            return;
        }
        self.errors.push(ValidationError {
            instance_path: self.path.clone(),
            keyword,
            message: message.into(),
        });
    }

    /// Applies node `index` to `value`. With `record`, failures are reported;
    /// without, the first failure returns at once.
    fn apply(&mut self, index: u32, value: &Value<'_>, depth: usize, record: bool) -> bool {
        if self.exhausted {
            return false;
        }
        self.steps += 1;
        if self.steps > self.limits.max_steps || depth > self.limits.max_validation_depth {
            self.exhausted = true;
            return false;
        }
        let nodes = self.nodes;
        match nodes.get(index as usize) {
            Some(Node::Bool(true)) => true,
            Some(Node::Bool(false)) => {
                self.record(record, "false", "no value is allowed here");
                false
            }
            Some(Node::Keywords(k)) => self.keywords(k, value, depth, record),
            // Indices come from the compiler and are always in range.
            None => false,
        }
    }

    fn keywords(&mut self, k: &'s Keywords, value: &Value<'_>, depth: usize, record: bool) -> bool {
        let mut valid = true;
        macro_rules! check {
            ($ok:expr) => {
                if !$ok {
                    valid = false;
                    if !record || self.exhausted {
                        return false;
                    }
                }
            };
        }
        check!(self.check_type(k, value, record));
        if k.constant.is_some() || k.enumeration.is_some() {
            check!(self.check_values(k, value, record));
        }
        match value {
            Value::Number(n) => check!(self.check_number(k, n.as_str(), record)),
            Value::String(s) => check!(self.check_string(k, s, record)),
            Value::Array(items) => check!(self.check_array(k, items, depth, record)),
            Value::Object(_) => check!(self.check_object(k, value, depth, record)),
            Value::Null | Value::Bool(_) => {}
        }
        if let Some(target) = k.reference {
            check!(self.apply(target, value, depth + 1, record));
        }
        for &sub in &k.all_of {
            check!(self.apply(sub, value, depth + 1, record));
        }
        if !k.any_of.is_empty() {
            let mut matched = false;
            for &sub in &k.any_of {
                if self.apply(sub, value, depth + 1, false) {
                    matched = true;
                    break;
                }
            }
            if !matched {
                self.record(
                    record,
                    "anyOf",
                    "does not match any of the alternatives in anyOf",
                );
            }
            check!(matched);
        }
        if !k.one_of.is_empty() {
            let mut matches = 0;
            for &sub in &k.one_of {
                if self.apply(sub, value, depth + 1, false) {
                    matches += 1;
                    if matches > 1 {
                        break;
                    }
                }
            }
            if matches != 1 {
                let message = if matches == 0 {
                    "does not match any of the alternatives in oneOf"
                } else {
                    "matches more than one of the alternatives in oneOf"
                };
                self.record(record, "oneOf", message);
            }
            check!(matches == 1);
        }
        if let Some(sub) = k.not {
            let matched = self.apply(sub, value, depth + 1, false);
            if matched {
                self.record(record, "not", "matches a schema it must not match");
            }
            check!(!matched && !self.exhausted);
        }
        if let Some(condition) = k.if_ {
            let branch = if self.apply(condition, value, depth + 1, false) {
                k.then
            } else {
                k.else_
            };
            if let Some(branch) = branch {
                check!(self.apply(branch, value, depth + 1, record));
            }
        }
        valid
    }

    /// `value`'s canonical text, charged to the step budget by its size (one
    /// step per 64 bytes), so `enum`, `const` and `uniqueItems` applied at
    /// many levels of a large instance cannot do unbounded work. `None` when
    /// the budget runs out or the value has no canonical text.
    fn canonical(&mut self, value: &Value<'_>) -> Option<String> {
        match canon::text(value) {
            Ok(text) => {
                self.steps = self
                    .steps
                    .saturating_add(1 + u64::try_from(text.len() / 64).unwrap_or(u64::MAX));
                if self.steps > self.limits.max_steps {
                    self.exhausted = true;
                    return None;
                }
                Some(text)
            }
            Err(canon::Uncanonical::TooDeep) => {
                self.exhausted = true;
                None
            }
            Err(canon::Uncanonical::NumberOutOfRange) => {
                self.out_of_range = true;
                None
            }
        }
    }

    fn check_type(&mut self, k: &Keywords, value: &Value<'_>, record: bool) -> bool {
        if k.types == 0 {
            return true;
        }
        let bits = match value {
            Value::Null => types::NULL,
            Value::Bool(_) => types::BOOLEAN,
            Value::Object(_) => types::OBJECT,
            Value::Array(_) => types::ARRAY,
            Value::String(_) => types::STRING,
            Value::Number(n) => match Decimal::parse(n.as_str()) {
                Some(d) if d.is_integer() => types::NUMBER | types::INTEGER,
                Some(_) => types::NUMBER,
                // Whether it is an integer cannot be told exactly; it is a
                // number either way.
                None if k.types & types::NUMBER != 0 => types::NUMBER,
                None => {
                    self.out_of_range = true;
                    return false;
                }
            },
        };
        if k.types & bits != 0 {
            return true;
        }
        let message = format!(
            "expected {}, found {}",
            types::names(k.types),
            value.kind().name()
        );
        self.record(record, "type", message);
        false
    }

    fn check_values(&mut self, k: &Keywords, value: &Value<'_>, record: bool) -> bool {
        let Some(text) = self.canonical(value) else {
            return false;
        };
        let mut valid = true;
        if let Some(constant) = &k.constant {
            if *constant != text {
                self.record(record, "const", "does not equal the required constant");
                valid = false;
            }
        }
        if let Some(values) = &k.enumeration {
            if values.binary_search(&text).is_err() {
                self.record(record, "enum", "is not one of the allowed values");
                valid = false;
            }
        }
        valid
    }

    fn check_number(&mut self, k: &Keywords, text: &str, record: bool) -> bool {
        let numeric = k.minimum.is_some()
            || k.maximum.is_some()
            || k.exclusive_minimum.is_some()
            || k.exclusive_maximum.is_some()
            || k.multiple_of.is_some();
        if !numeric {
            return true;
        }
        let Some(d) = Decimal::parse(text) else {
            self.out_of_range = true;
            return false;
        };
        let mut valid = true;
        let bounds: [Bound<'_>; 4] = [
            (&k.minimum, "minimum", |v, b| v >= b, "at least"),
            (&k.maximum, "maximum", |v, b| v <= b, "at most"),
            (
                &k.exclusive_minimum,
                "exclusiveMinimum",
                |v, b| v > b,
                "greater than",
            ),
            (
                &k.exclusive_maximum,
                "exclusiveMaximum",
                |v, b| v < b,
                "less than",
            ),
        ];
        for (bound, keyword, holds, phrase) in bounds {
            if let Some(b) = bound {
                if !holds(&d, b) {
                    let mut limit = String::new();
                    b.write_canonical(&mut limit);
                    self.record(record, keyword, format!("must be {phrase} {limit}"));
                    valid = false;
                    if !record {
                        return false;
                    }
                }
            }
        }
        if let Some(divisor) = &k.multiple_of {
            if !d.is_multiple_of(divisor) {
                let mut limit = String::new();
                divisor.write_canonical(&mut limit);
                self.record(
                    record,
                    "multipleOf",
                    format!("must be a multiple of {limit}"),
                );
                valid = false;
            }
        }
        valid
    }

    fn check_string(&mut self, k: &Keywords, s: &str, record: bool) -> bool {
        if k.min_length.is_none() && k.max_length.is_none() {
            return true;
        }
        // Length counts Unicode code points, not bytes or UTF-16 units.
        let length = u64::try_from(s.chars().count()).unwrap_or(u64::MAX);
        let mut valid = true;
        if let Some(min) = k.min_length.filter(|&min| length < min) {
            self.record(
                record,
                "minLength",
                format!("must be at least {min} characters long"),
            );
            valid = false;
        }
        if let Some(max) = k.max_length.filter(|&max| length > max) {
            self.record(
                record,
                "maxLength",
                format!("must be at most {max} characters long"),
            );
            valid = false;
        }
        valid
    }

    fn check_array(
        &mut self,
        k: &'s Keywords,
        items: &[Value<'_>],
        depth: usize,
        record: bool,
    ) -> bool {
        let mut valid = true;
        let len = u64::try_from(items.len()).unwrap_or(u64::MAX);
        if let Some(min) = k.min_items.filter(|&min| len < min) {
            self.record(
                record,
                "minItems",
                format!("must have at least {min} items"),
            );
            valid = false;
        }
        if let Some(max) = k.max_items.filter(|&max| len > max) {
            self.record(record, "maxItems", format!("must have at most {max} items"));
            valid = false;
        }
        if !valid && !record {
            return false;
        }
        if k.unique_items && !self.unique(items) {
            self.record(record, "uniqueItems", "must not contain duplicate items");
            if !record {
                return false;
            }
            valid = false;
        }
        for (i, item) in items.iter().enumerate() {
            let schema = match k.prefix_items.get(i) {
                Some(&schema) => Some(schema),
                None => k.items,
            };
            if let Some(schema) = schema {
                let mark = self.path.len();
                push_index(&mut self.path, i);
                let ok = self.apply(schema, item, depth + 1, record);
                self.path.truncate(mark);
                if !ok {
                    valid = false;
                    if !record || self.exhausted {
                        return false;
                    }
                }
            }
        }
        if let Some(contains) = k.contains {
            let mut matches: u64 = 0;
            for item in items.iter() {
                if self.apply(contains, item, depth + 1, false) {
                    matches += 1;
                }
                if self.exhausted {
                    return false;
                }
            }
            let min = k.min_contains.unwrap_or(1);
            if matches < min {
                self.record(
                    record,
                    "contains",
                    format!("must contain at least {min} matching items"),
                );
                valid = false;
            }
            if let Some(max) = k.max_contains.filter(|&max| matches > max) {
                self.record(
                    record,
                    "maxContains",
                    format!("must contain at most {max} matching items"),
                );
                valid = false;
            }
        }
        valid
    }

    /// Whether `items` holds no two equal values, by sorting their canonical
    /// texts: `O(n log n)` in the items' total size, not `O(n^2)`.
    fn unique(&mut self, items: &[Value<'_>]) -> bool {
        let mut texts = Vec::with_capacity(items.len());
        for item in items {
            match self.canonical(item) {
                Some(text) => texts.push(text),
                None => return false,
            }
        }
        texts.sort_unstable();
        !texts.windows(2).any(|w| w[0] == w[1])
    }

    fn check_object(
        &mut self,
        k: &'s Keywords,
        value: &Value<'_>,
        depth: usize,
        record: bool,
    ) -> bool {
        let Some(object) = value.as_object() else {
            return true;
        };
        let mut valid = true;
        macro_rules! fail {
            () => {{
                valid = false;
                if !record || self.exhausted {
                    return false;
                }
            }};
        }
        let len = u64::try_from(object.len()).unwrap_or(u64::MAX);
        if let Some(min) = k.min_properties.filter(|&min| len < min) {
            self.record(
                record,
                "minProperties",
                format!("must have at least {min} properties"),
            );
            fail!();
        }
        if let Some(max) = k.max_properties.filter(|&max| len > max) {
            self.record(
                record,
                "maxProperties",
                format!("must have at most {max} properties"),
            );
            fail!();
        }
        for name in &k.required {
            if !object.contains_key(name) {
                self.record(
                    record,
                    "required",
                    format!("is missing the required property {}", quoted(name)),
                );
                fail!();
            }
        }
        for (name, needed) in &k.dependent_required {
            if object.contains_key(name) {
                for other in needed {
                    if !object.contains_key(other) {
                        self.record(
                            record,
                            "dependentRequired",
                            format!(
                                "must have the property {} when {} is present",
                                quoted(other),
                                quoted(name)
                            ),
                        );
                        fail!();
                    }
                }
            }
        }
        for (key, member) in object.iter() {
            if let Some(names) = k.property_names {
                let key_value = Value::String(Cow::Borrowed(key));
                let ok = self.apply(names, &key_value, depth + 1, false);
                if !ok {
                    self.record(
                        record,
                        "propertyNames",
                        format!(
                            "has a property name the schema does not allow: {}",
                            quoted(key)
                        ),
                    );
                    fail!();
                }
            }
            let schema = match k
                .properties
                .binary_search_by(|(name, _)| name.as_ref().cmp(key))
            {
                Ok(at) => k.properties.get(at).map(|(_, schema)| *schema),
                Err(_) => k.additional_properties,
            };
            if let Some(schema) = schema {
                let mark = self.path.len();
                push_key(&mut self.path, key);
                let recorded = self.errors.len();
                let ok = self.apply(schema, member, depth + 1, record);
                self.path.truncate(mark);
                if !ok {
                    if k.properties
                        .binary_search_by(|(name, _)| name.as_ref().cmp(key))
                        .is_err()
                        && matches!(self.nodes.get(schema as usize), Some(Node::Bool(false)))
                        && self.errors.len() > recorded
                    {
                        // Just recorded by the `false` node under the
                        // member's path; say what it means.
                        if let Some(last) = self.errors.last_mut() {
                            if last.keyword == "false" {
                                last.keyword = "additionalProperties";
                                last.message = "is not an allowed property".to_owned();
                            }
                        }
                    }
                    fail!();
                }
            }
        }
        for (name, schema) in &k.dependent_schemas {
            if object.contains_key(name) && !self.apply(*schema, value, depth + 1, record) {
                fail!();
            }
        }
        valid
    }
}

fn quoted(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    crate::jsonx::write_string(&mut out, name);
    out
}

/// Appends one JSON Pointer token (RFC 6901 escaping) to the path.
fn push_key(path: &mut String, key: &str) {
    path.push('/');
    for c in key.chars() {
        match c {
            '~' => path.push_str("~0"),
            '/' => path.push_str("~1"),
            c => path.push(c),
        }
    }
}

fn push_index(path: &mut String, index: usize) {
    use core::fmt::Write as _;
    path.push('/');
    let _infallible = write!(path, "{index}");
}
