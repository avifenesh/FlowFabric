//! Parse Valkey FCALL return values into structured results.
//!
//! Lua return convention (RFC-010 §4.9):
//!   Success: {1, "OK", ...values}       or {1, "ALREADY_SATISFIED", ...}
//!   Failure: {0, "ERROR_NAME", ...context}
//!
//! The FCALL result comes back as a ferriskey `Value::Array`.

use crate::error::ScriptError;
use ferriskey::Value;

/// Parsed FCALL result from a FlowFabric Lua function.
#[derive(Debug)]
pub struct FcallResult {
    /// 1 = success, 0 = failure.
    pub success: bool,
    /// Status string: "OK", "ALREADY_SATISFIED", "DUPLICATE", or error code.
    pub status: String,
    /// Remaining fields after status code and status string.
    pub fields: Vec<Value>,
}

impl FcallResult {
    /// Parse a raw `Value` returned by `FCALL` into a structured result.
    pub fn parse(raw: &Value) -> Result<Self, ScriptError> {
        let items = match raw {
            Value::Array(arr) => arr,
            // ff_version returns a bare string, not an array.
            // Individual callers handle that; this parser is for the
            // standard {status_code, status_string, ...} convention.
            _ => {
                return Err(ScriptError::Parse(format!(
                    "expected Array, got {:?}",
                    value_type_name(raw)
                )));
            }
        };

        if items.is_empty() {
            return Err(ScriptError::Parse("empty FCALL result array".into()));
        }

        // Element [0]: status code (Int 1 or 0)
        let status_code = match items.first() {
            Some(Ok(Value::Int(n))) => *n,
            other => {
                return Err(ScriptError::Parse(format!(
                    "expected Int at index 0, got {:?}",
                    other
                )));
            }
        };

        // Element [1]: status string
        let status = if items.len() > 1 {
            value_to_string(items[1].as_ref().ok())
        } else {
            String::new()
        };

        // Remaining elements
        let fields: Vec<Value> = items
            .iter()
            .skip(2)
            .filter_map(|r| r.as_ref().ok().cloned())
            .collect();

        Ok(FcallResult {
            success: status_code == 1,
            status,
            fields,
        })
    }

    /// If this is a failure result, convert the status string to a ScriptError.
    /// Returns Ok(self) if success.
    pub fn into_success(self) -> Result<Self, ScriptError> {
        if self.success {
            Ok(self)
        } else {
            Err(ScriptError::from_code(&self.status).unwrap_or_else(|| {
                ScriptError::Parse(format!("unknown error code: {}", self.status))
            }))
        }
    }

    /// Get a field as a string, or empty string if missing.
    pub fn field_str(&self, index: usize) -> String {
        self.fields
            .get(index)
            .map(|v| value_to_string(Some(v)))
            .unwrap_or_default()
    }
}

/// Trait for converting a raw FCALL `Value` into a typed result.
///
/// Each contract Result type (e.g. `CreateExecutionResult`, `CompleteExecutionResult`)
/// implements this trait to parse the Lua return into the appropriate Rust enum variant.
pub trait FromFcallResult: Sized {
    fn from_fcall_result(raw: &Value) -> Result<Self, ScriptError>;
}

/// Extract a string from a Value.
fn value_to_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
        Some(Value::SimpleString(s)) => s.clone(),
        Some(Value::Int(n)) => n.to_string(),
        Some(Value::Okay) => "OK".into(),
        _ => String::new(),
    }
}

/// Type name for error messages.
fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Nil => "Nil",
        Value::Int(_) => "Int",
        Value::BulkString(_) => "BulkString",
        Value::Array(_) => "Array",
        Value::SimpleString(_) => "SimpleString",
        Value::Okay => "Okay",
        Value::Map(_) => "Map",
        Value::Set(_) => "Set",
        Value::Double(_) => "Double",
        Value::Boolean(_) => "Boolean",
        Value::BigNumber(_) => "BigNumber",
        Value::VerbatimString { .. } => "VerbatimString",
        Value::Attribute { .. } => "Attribute",
        Value::Push { .. } => "Push",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_value(fields: Vec<Value>) -> Value {
        let mut arr: Vec<Result<Value, ferriskey::Error>> = vec![
            Ok(Value::Int(1)),
            Ok(Value::BulkString("OK".into())),
        ];
        for f in fields {
            arr.push(Ok(f));
        }
        Value::Array(arr)
    }

    fn err_value(code: &str) -> Value {
        Value::Array(vec![
            Ok(Value::Int(0)),
            Ok(Value::BulkString(Vec::from(code.as_bytes()).into())),
        ])
    }

    #[test]
    fn parse_success() {
        let raw = ok_value(vec![Value::BulkString("hello".into())]);
        let result = FcallResult::parse(&raw).unwrap();
        assert!(result.success);
        assert_eq!(result.status, "OK");
        assert_eq!(result.fields.len(), 1);
        assert_eq!(result.field_str(0), "hello");
    }

    #[test]
    fn parse_error() {
        let raw = err_value("stale_lease");
        let result = FcallResult::parse(&raw).unwrap();
        assert!(!result.success);
        assert_eq!(result.status, "stale_lease");
    }

    #[test]
    fn into_success_ok() {
        let raw = ok_value(vec![]);
        let result = FcallResult::parse(&raw).unwrap().into_success();
        assert!(result.is_ok());
    }

    #[test]
    fn into_success_err() {
        let raw = err_value("lease_expired");
        let result = FcallResult::parse(&raw).unwrap().into_success();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ScriptError::LeaseExpired));
    }

    #[test]
    fn parse_non_array_fails() {
        let raw = Value::SimpleString("hello".into());
        let result = FcallResult::parse(&raw);
        assert!(result.is_err());
    }
}
