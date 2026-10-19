//! Building the JSON text of results and notifications.
//!
//! Results are written straight into a `String` (strings escaped by
//! jsonx-lib's writer), with pre-rendered pieces such as tool definitions
//! copied in verbatim, rather than assembled as a value tree and serialized.

use crate::{
    jsonx::{self, Value},
    rpc_io::{ErrorObject, Raw},
};

/// A JSON object being written member by member.
pub(crate) struct Obj {
    out: String,
    empty: bool,
}

impl Obj {
    pub(crate) fn new() -> Self {
        Self::with_capacity(64)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let mut out = String::with_capacity(capacity);
        out.push('{');
        Self { out, empty: true }
    }

    fn key(&mut self, key: &str) {
        if !self.empty {
            self.out.push(',');
        }
        self.empty = false;
        jsonx::write_string(&mut self.out, key);
        self.out.push(':');
    }

    /// A member whose value is already JSON text.
    pub(crate) fn raw(&mut self, key: &str, json: &str) -> &mut Self {
        self.key(key);
        self.out.push_str(json);
        self
    }

    pub(crate) fn str(&mut self, key: &str, value: &str) -> &mut Self {
        self.key(key);
        jsonx::write_string(&mut self.out, value);
        self
    }

    pub(crate) fn opt_str(&mut self, key: &str, value: Option<&str>) -> &mut Self {
        if let Some(value) = value {
            let _same = self.str(key, value);
        }
        self
    }

    pub(crate) fn bool(&mut self, key: &str, value: bool) -> &mut Self {
        self.raw(key, if value { "true" } else { "false" })
    }

    pub(crate) fn u64(&mut self, key: &str, value: u64) -> &mut Self {
        self.key(key);
        self.out.push_str(&value.to_string());
        self
    }

    pub(crate) fn value(&mut self, key: &str, value: &Value<'_>) -> &mut Self {
        self.key(key);
        jsonx::write_value(&mut self.out, value);
        self
    }

    /// A member whose value is written by `f` into the buffer.
    pub(crate) fn with(&mut self, key: &str, f: impl FnOnce(&mut String)) -> &mut Self {
        self.key(key);
        f(&mut self.out);
        self
    }

    pub(crate) fn finish(mut self) -> String {
        self.out.push('}');
        self.out
    }
}

/// `text` (built by this crate, so valid JSON) as a value to send.
///
/// rpc-io validates raw text once more on the way in; a failure would be a
/// bug here, answered as an internal error rather than sending bad JSON.
pub(crate) fn raw(text: String) -> Result<Raw<'static>, ErrorObject> {
    Raw::from_string(text)
        .map_err(|_| ErrorObject::internal_error("the server built an invalid result"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_obj_writes_members_in_order_with_escaping() {
        let mut o = Obj::new();
        let _ = o
            .str("a\"b", "x\ny")
            .u64("n", 7)
            .bool("t", true)
            .opt_str("none", None)
            .raw("r", "[1]")
            .with("w", |out| out.push_str("null"));
        assert_eq!(
            o.finish(),
            r#"{"a\"b":"x\ny","n":7,"t":true,"r":[1],"w":null}"#
        );
        assert_eq!(Obj::new().finish(), "{}");
    }

    #[test]
    fn test_raw_refuses_invalid_text() {
        assert!(raw("{".to_owned()).is_err());
        assert!(raw("{}".to_owned()).is_ok());
    }
}
