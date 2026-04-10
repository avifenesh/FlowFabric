// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0
//! Unified error handling for FlowFabric Glide-Core
//!
//! This module provides a comprehensive error type hierarchy that consolidates
//! all error sources across the codebase into a single, structured error enum.
//! This enables better error handling, context preservation, and backward compatibility.

use redis::RedisError;
use std::fmt;
use std::io;

/// The primary error type for all FlowFabric operations
#[derive(Debug)]
pub enum FlowFabricError {
    /// Redis/Valkey connection or command error
    Redis(RedisError),

    /// Connection-related errors (timeout, disconnect, failed auth)
    Connection {
        reason: String,
        recoverable: bool,
    },

    /// Compression/decompression operation failed
    Compression {
        backend: String,
        operation: String,
        message: String,
    },

    /// AWS IAM authentication failed
    IamAuth {
        message: String,
        recoverable: bool,
    },

    /// Configuration or parameter validation error
    Configuration(String),

    /// Timeout during operation
    Timeout {
        operation: String,
        duration_ms: u32,
    },

    /// Invalid command or arguments
    InvalidCommand(String),

    /// PubSub-related error
    PubSub(String),

    /// Internal error (should not occur in normal operation)
    Internal(String),

    /// I/O error
    Io(io::Error),
}

impl FlowFabricError {
    /// Create a new connection error
    pub fn connection(reason: impl Into<String>, recoverable: bool) -> Self {
        FlowFabricError::Connection {
            reason: reason.into(),
            recoverable,
        }
    }

    /// Create a new compression error
    pub fn compression(backend: impl Into<String>, operation: impl Into<String>, message: impl Into<String>) -> Self {
        FlowFabricError::Compression {
            backend: backend.into(),
            operation: operation.into(),
            message: message.into(),
        }
    }

    /// Create a new IAM auth error
    pub fn iam_auth(message: impl Into<String>, recoverable: bool) -> Self {
        FlowFabricError::IamAuth {
            message: message.into(),
            recoverable,
        }
    }

    /// Create a new timeout error
    pub fn timeout(operation: impl Into<String>, duration_ms: u32) -> Self {
        FlowFabricError::Timeout {
            operation: operation.into(),
            duration_ms,
        }
    }

    /// Check if this error is recoverable (should retry)
    pub fn is_recoverable(&self) -> bool {
        match self {
            FlowFabricError::Redis(err) => {
                // Delegate to redis-rs error classification
                !err.is_unrecoverable_error()
            }
            FlowFabricError::Connection { recoverable, .. } => *recoverable,
            FlowFabricError::Timeout { .. } => true,
            FlowFabricError::IamAuth { recoverable, .. } => *recoverable,
            FlowFabricError::Compression { .. } => false,
            FlowFabricError::Configuration(_) => false,
            FlowFabricError::InvalidCommand(_) => false,
            FlowFabricError::PubSub(_) => false,
            FlowFabricError::Internal(_) => false,
            FlowFabricError::Io(_) => true,
        }
    }

    /// Classify this error for external error reporting (backward compat with RequestErrorType)
    pub fn classify(&self) -> ErrorClassification {
        match self {
            FlowFabricError::Timeout { .. } => ErrorClassification::Timeout,
            FlowFabricError::Connection { .. } | FlowFabricError::Io(_) => ErrorClassification::Disconnect,
            FlowFabricError::Redis(err) if matches!(err.kind(), redis::ErrorKind::ExecAbortError) => {
                ErrorClassification::ExecAbort
            }
            _ => ErrorClassification::Unspecified,
        }
    }
}

/// Error classification for external error reporting (backward compatible with RequestErrorType)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClassification {
    Unspecified = 0,
    ExecAbort = 1,
    Timeout = 2,
    Disconnect = 3,
}

impl fmt::Display for FlowFabricError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlowFabricError::Redis(err) => write!(f, "Redis error: {}", err),
            FlowFabricError::Connection { reason, recoverable } => {
                if *recoverable {
                    write!(f, "Connection error (recoverable): {}. Will attempt to reconnect", reason)
                } else {
                    write!(f, "Connection error (fatal): {}", reason)
                }
            }
            FlowFabricError::Compression { backend, operation, message } => {
                write!(f, "Compression error ({} - {}): {}", backend, operation, message)
            }
            FlowFabricError::IamAuth { message, .. } => write!(f, "IAM authentication error: {}", message),
            FlowFabricError::Configuration(msg) => write!(f, "Configuration error: {}", msg),
            FlowFabricError::Timeout { operation, duration_ms } => {
                write!(f, "Timeout after {}ms during {}", duration_ms, operation)
            }
            FlowFabricError::InvalidCommand(msg) => write!(f, "Invalid command: {}", msg),
            FlowFabricError::PubSub(msg) => write!(f, "PubSub error: {}", msg),
            FlowFabricError::Internal(msg) => write!(f, "Internal error: {}", msg),
            FlowFabricError::Io(err) => write!(f, "I/O error: {}", err),
        }
    }
}

impl std::error::Error for FlowFabricError {}

// Backward compatibility conversions

/// Convert from RedisError (from redis-rs)
impl From<RedisError> for FlowFabricError {
    fn from(err: RedisError) -> Self {
        FlowFabricError::Redis(err)
    }
}

/// Convert from io::Error
impl From<io::Error> for FlowFabricError {
    fn from(err: io::Error) -> Self {
        FlowFabricError::Io(err)
    }
}

/// Convert from String
impl From<String> for FlowFabricError {
    fn from(msg: String) -> Self {
        FlowFabricError::Internal(msg)
    }
}

/// Convert from &str
impl From<&str> for FlowFabricError {
    fn from(msg: &str) -> Self {
        FlowFabricError::Internal(msg.to_string())
    }
}

/// Result type using FlowFabricError
pub type FlowFabricResult<T> = Result<T, FlowFabricError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_is_recoverable() {
        let err = FlowFabricError::timeout("SELECT", 250);
        assert!(err.is_recoverable());
    }

    #[test]
    fn test_connection_recoverable_classification() {
        let err = FlowFabricError::connection("Connection dropped", true);
        assert_eq!(err.classify(), ErrorClassification::Disconnect);
    }

    #[test]
    fn test_display_format() {
        let err = FlowFabricError::timeout("GET key", 500);
        let msg = format!("{}", err);
        assert!(msg.contains("500ms"));
        assert!(msg.contains("GET key"));
    }
}
