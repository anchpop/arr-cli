//! Dict-poking helpers over serde_json::Value, mirroring how arr.py leans on
//! `.get(...)` chains against the loosely-typed arr APIs (fields appear and
//! disappear between versions, so we default instead of erroring).

use serde_json::Value;

pub trait JsonExt {
    /// String field, "" when missing/not-a-string.
    fn s(&self, key: &str) -> &str;
    /// Integer field, 0 when missing.
    fn i(&self, key: &str) -> i64;
    /// Float field, 0.0 when missing (accepts ints too).
    fn f(&self, key: &str) -> f64;
    /// Bool field, false when missing.
    fn b(&self, key: &str) -> bool;
    /// Array field, empty slice when missing.
    fn a(&self, key: &str) -> &[Value];
    /// Nested object lookup: `v.at(&["quality", "quality", "name"])`.
    fn at(&self, path: &[&str]) -> &Value;
    /// Present-and-non-null check.
    fn has(&self, key: &str) -> bool;
}

static NULL: Value = Value::Null;

impl JsonExt for Value {
    fn s(&self, key: &str) -> &str {
        self.get(key).and_then(Value::as_str).unwrap_or("")
    }
    fn i(&self, key: &str) -> i64 {
        self.get(key)
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .unwrap_or(0)
    }
    fn f(&self, key: &str) -> f64 {
        self.get(key).and_then(Value::as_f64).unwrap_or(0.0)
    }
    fn b(&self, key: &str) -> bool {
        self.get(key).and_then(Value::as_bool).unwrap_or(false)
    }
    fn a(&self, key: &str) -> &[Value] {
        self.get(key).and_then(Value::as_array).map(|v| v.as_slice()).unwrap_or(&[])
    }
    fn at(&self, path: &[&str]) -> &Value {
        let mut cur = self;
        for k in path {
            cur = cur.get(k).unwrap_or(&NULL);
        }
        cur
    }
    fn has(&self, key: &str) -> bool {
        matches!(self.get(key), Some(v) if !v.is_null())
    }
}

/// Iterate an array-valued response (empty when the response wasn't an array).
pub fn items(v: &Option<Value>) -> &[Value] {
    v.as_ref().and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[])
}
