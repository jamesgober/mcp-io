//! Canonical JSON text for JSON Schema equality.
//!
//! JSON Schema compares instances by value: numbers by mathematical value
//! (`1` equals `1.0`), objects regardless of member order, strings by
//! content. jsonx-lib's `Value` equality is textual and ordered, so `enum`,
//! `const` and `uniqueItems` compare canonical text instead: equal values,
//! and only equal values, write equal text. `uniqueItems` then needs a sort,
//! not a pairwise comparison, so it stays `O(n log n)`.

use crate::jsonx::Value;

use super::decimal::Decimal;

/// Deeper values are refused rather than written: canonical writing recurses
/// once per level, and the budget keeps that bounded for values built in
/// code, which no parser depth limit has checked.
pub(crate) const MAX_CANONICAL_DEPTH: usize = 512;

/// Why a value has no canonical text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Uncanonical {
    /// Deeper than [`MAX_CANONICAL_DEPTH`].
    TooDeep,
    /// It holds a number outside the exact range (see `Decimal`).
    NumberOutOfRange,
}

/// Writes `value`'s canonical text into `out`.
pub(crate) fn write(out: &mut String, value: &Value<'_>) -> Result<(), Uncanonical> {
    write_at(out, value, 0)
}

/// The canonical text of `value`.
pub(crate) fn text(value: &Value<'_>) -> Result<String, Uncanonical> {
    let mut out = String::new();
    write(&mut out, value).map(|()| out)
}

fn write_at(out: &mut String, value: &Value<'_>, depth: usize) -> Result<(), Uncanonical> {
    if depth > MAX_CANONICAL_DEPTH {
        return Err(Uncanonical::TooDeep);
    }
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => match Decimal::parse(n.as_str()) {
            Some(d) => d.write_canonical(out),
            None => return Err(Uncanonical::NumberOutOfRange),
        },
        Value::String(s) => crate::jsonx::write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_at(out, item, depth + 1)?;
            }
            out.push(']');
        }
        Value::Object(object) => {
            let mut members: Vec<(&str, &Value<'_>)> = object.iter().collect();
            members.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, member)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                crate::jsonx::write_string(out, key);
                out.push(':');
                write_at(out, member, depth + 1)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::jsonx::parse;

    fn canon(text: &str) -> String {
        super::text(&parse(text).unwrap()).unwrap()
    }

    #[test]
    fn test_canonical_equal_values_write_equal_text() {
        assert_eq!(canon("1"), canon("1.0"));
        assert_eq!(
            canon(r#"{"a":1,"b":[2.50]}"#),
            canon(r#"{"b":[25e-1],"a":1.0}"#)
        );
        assert_eq!(canon(r#""A""#), canon(r#""A""#));
    }

    #[test]
    fn test_canonical_different_values_write_different_text() {
        assert_ne!(canon("1"), canon(r#""1""#));
        assert_ne!(canon("[1,2]"), canon("[2,1]"));
        assert_ne!(canon(r#"{"a":null}"#), canon("{}"));
        assert_ne!(canon("true"), canon("1"));
        // A key that would read like a separator cannot fake a member.
        assert_ne!(canon(r#"{"a\":1,\"b":2}"#), canon(r#"{"a":1,"b":2}"#));
    }

    #[test]
    fn test_canonical_depth_is_bounded() {
        let mut deep = Value::Null;
        for _ in 0..=MAX_CANONICAL_DEPTH {
            deep = Value::Array(vec![deep].into());
        }
        assert_eq!(super::text(&deep), Err(Uncanonical::TooDeep));
        let huge = parse("[1e9999999999999999999]").unwrap();
        assert_eq!(super::text(&huge), Err(Uncanonical::NumberOutOfRange));
    }
}
