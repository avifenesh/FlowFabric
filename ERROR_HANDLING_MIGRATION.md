# FlowFabric Error Handling Migration Guide

## Overview

A unified error handling system has been implemented to consolidate error handling patterns across the codebase. This replaces the previous mixed approach (RedisResult<T>, CompressionResult<T>, Result<T, String>) with a single, structured error type.

## New Error Types

### FlowFabricError (Primary Error Type)

A comprehensive enum covering all error sources:

```rust
pub enum FlowFabricError {
    Redis(RedisError),                        // Redis-rs errors
    Connection { reason, recoverable },        // Connection failures
    Compression { backend, operation, message },// Compression errors
    IamAuth { message, recoverable },          // AWS IAM auth errors
    Configuration(String),                     // Configuration errors
    Timeout { operation, duration_ms },        // Operation timeouts
    InvalidCommand(String),                    // Invalid command errors
    PubSub(String),                            // Pub/Sub errors
    Internal(String),                          // Internal errors
    Io(io::Error),                             // I/O errors
}
```

### FlowFabricResult<T> (Result Type Alias)

```rust
pub type FlowFabricResult<T> = Result<T, FlowFabricError>;
```

### ErrorClassification (Backward Compatible)

Equivalent to the old RequestErrorType:

```rust
pub enum ErrorClassification {
    Unspecified = 0,
    ExecAbort = 1,
    Timeout = 2,
    Disconnect = 3,
}
```

## Key Features

### 1. Structured Error Information

```rust
// OLD: Lost context with string errors
Err("Connection timeout".to_string())

// NEW: Rich context preserved
Err(FlowFabricError::timeout("SELECT db 0", 2000))
// Error: Timeout after 2000ms during SELECT db 0
```

### 2. Recoverable Error Classification

```rust
let err = FlowFabricError::connection("Network unreachable", true);
if err.is_recoverable() {
    // Implement retry logic
}
```

### 3. Error Classification for External Reporting

```rust
let err = FlowFabricError::timeout("GET key", 250);
let classification = err.classify();
match classification {
    ErrorClassification::Timeout => { /* handle timeout */ },
    ErrorClassification::Disconnect => { /* handle disconnect */ },
    ErrorClassification::ExecAbort => { /* handle exec abort */ },
    ErrorClassification::Unspecified => { /* handle generic */ },
}
```

### 4. Human-Readable Error Messages

```rust
impl Display for FlowFabricError {
    // Automatically formats errors with context
}

// Example:
// "Connection error (recoverable): Network dropped. Will attempt to reconnect"
// "Timeout after 500ms during GET my_key"
// "Compression error (lz4 - decompress): Invalid compression format"
```

## Migration Path

### For Users

Most users won't need to change code due to automatic From conversions:

```rust
// OLD: Direct RedisError
let result: Result<Value, RedisError> = client.execute().await;

// NEW: FlowFabricError (automatic conversion from RedisError)
let result: FlowFabricResult<Value> = client.execute().await;
```

### For Error Handling

```rust
// OLD: Pattern matching on error types
match err {
    RedisError => { /* handle redis */ },
    CompressionError => { /* handle compression */ },
    Err::<_, String> => { /* handle generic */ },
}

// NEW: Single pattern matching
match err {
    FlowFabricError::Redis(redis_err) => { /* handle redis */ },
    FlowFabricError::Compression { .. } => { /* handle compression */ },
    FlowFabricError::Connection { recoverable, .. } => {
        if recoverable {
            // retry
        }
    },
}
```

## Backward Compatibility

### Automatic Conversions

```rust
// RedisError automatically converts to FlowFabricError::Redis
let redis_err: RedisError = /* ... */;
let ff_err: FlowFabricError = redis_err.into();

// String automatically converts to FlowFabricError::Internal
let msg = "Some error".to_string();
let ff_err: FlowFabricError = msg.into();

// io::Error automatically converts to FlowFabricError::Io
let io_err: io::Error = /* ... */;
let ff_err: FlowFabricError = io_err.into();
```

### RequestErrorType Compatibility

The old RequestErrorType enum is now replaced by ErrorClassification:

```rust
// OLD: RequestErrorType enum
pub enum RequestErrorType {
    Unspecified = 0,
    ExecAbort = 1,
    Timeout = 2,
    Disconnect = 3,
}

// NEW: ErrorClassification + classify() method
let err = FlowFabricError::timeout("GET", 250);
let classification = err.classify();
assert_eq!(classification, ErrorClassification::Timeout);
```

## Migration Checklist

For library maintainers updating code:

- [ ] Add `mod errors_unified;` to lib.rs
- [ ] Update error types from `RedisError` to `FlowFabricError`
- [ ] Update result types from `RedisResult<T>` to `FlowFabricResult<T>`
- [ ] Replace error construction with specific variants (e.g., `FlowFabricError::timeout()`)
- [ ] Update error handling to use `is_recoverable()` for retry logic
- [ ] Update tests to match new error types
- [ ] Add integration tests for error classification

## Example Conversions

### Connection Error

```rust
// OLD
Err(RedisError::io_error("Connection failed"))

// NEW
Err(FlowFabricError::connection("Connection failed", true))
```

### Compression Error

```rust
// OLD
Err(CompressionError::new("LZ4 decompression failed"))

// NEW
Err(FlowFabricError::compression("lz4", "decompress", "Invalid compression format"))
```

### Configuration Error

```rust
// OLD
Err("Invalid database ID".to_string())

// NEW
Err(FlowFabricError::Configuration("Invalid database ID".to_string()))
```

### Timeout Error

```rust
// OLD
Err("Operation timed out after 250ms".to_string())

// NEW
Err(FlowFabricError::timeout("GET key", 250))
```

## Benefits

1. **Type Safety** - Compiler catches error type mismatches
2. **Context Preservation** - Rich error information available
3. **Consistent Handling** - Single error type across codebase
4. **Better Messages** - User-facing error messages are clear and actionable
5. **Retry Logic** - Built-in `is_recoverable()` method
6. **External Compatibility** - `classify()` method for backward compatibility

## Timeline

- **Phase 1** (Now): errors_unified.rs added as new module (non-breaking)
- **Phase 2** (Next): Gradual migration of existing code to use FlowFabricError
- **Phase 3** (v2.0): Deprecate old error types, make FlowFabricError the default
- **Phase 4** (v3.0): Remove old error types entirely

