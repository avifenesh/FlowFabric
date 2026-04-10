use std::io::{self, Read};

use crate::valkey::types::{
    ErrorKind, PushKind, RedisError, RedisResult, ServerError, ServerErrorKind, Value,
    VerbatimFormat,
};

use bytes::{Buf, BytesMut};
use logger_core::log_error;
use num_bigint::BigInt;
use telemetrylib::GlideOpenTelemetry;

const MAX_RECURSE_DEPTH: usize = 100;

fn err_parser(line: &str) -> ServerError {
    let mut pieces = line.splitn(2, ' ');
    let kind = match pieces.next().unwrap() {
        "ERR" => ServerErrorKind::ResponseError,
        "EXECABORT" => ServerErrorKind::ExecAbortError,
        "LOADING" => ServerErrorKind::BusyLoadingError,
        "NOSCRIPT" => ServerErrorKind::NoScriptError,
        "MOVED" => {
            if let Err(e) = GlideOpenTelemetry::record_moved_error() {
                log_error(
                    "OpenTelemetry:moved_error",
                    format!("Failed to record moved error: {e}"),
                );
            }
            ServerErrorKind::Moved
        }
        "ASK" => ServerErrorKind::Ask,
        "TRYAGAIN" => ServerErrorKind::TryAgain,
        "CLUSTERDOWN" => ServerErrorKind::ClusterDown,
        "CROSSSLOT" => ServerErrorKind::CrossSlot,
        "MASTERDOWN" => ServerErrorKind::MasterDown,
        "READONLY" => ServerErrorKind::ReadOnly,
        "NOTBUSY" => ServerErrorKind::NotBusy,
        "NOPERM" => ServerErrorKind::PermissionDenied,
        code => {
            return ServerError::ExtensionError {
                code: code.to_string(),
                detail: pieces.next().map(|str| str.to_string()),
            }
        }
    };
    let detail = pieces.next().map(|str| str.to_string());
    ServerError::KnownError { kind, detail }
}

pub fn get_push_kind(kind: String) -> PushKind {
    match kind.as_str() {
        "invalidate" => PushKind::Invalidate,
        "message" => PushKind::Message,
        "pmessage" => PushKind::PMessage,
        "smessage" => PushKind::SMessage,
        "unsubscribe" => PushKind::Unsubscribe,
        "punsubscribe" => PushKind::PUnsubscribe,
        "sunsubscribe" => PushKind::SUnsubscribe,
        "subscribe" => PushKind::Subscribe,
        "psubscribe" => PushKind::PSubscribe,
        "ssubscribe" => PushKind::SSubscribe,
        _ => PushKind::Other(kind),
    }
}

// ── Hand-written RESP parser ──────────────────────────────────────────
//
// Operates directly on BytesMut. Returns Ok(None) when the buffer
// doesn't contain a complete message (partial read). Uses
// BytesMut::split_to() for zero-copy BulkString extraction.

/// Find the position of \r\n in the buffer starting from `start`.
/// Returns the byte offset of the '\r', or None if not found.
#[inline]
fn find_crlf(buf: &[u8], start: usize) -> Option<usize> {
    // memchr is typically faster for single-byte search, but \r\n is
    // two bytes. The simple loop is fine — lines are short (typically < 20 bytes).
    let slice = &buf[start..];
    // Use memchr for the first byte, then verify the second
    let mut pos = 0;
    while pos < slice.len().saturating_sub(1) {
        if slice[pos] == b'\r' && slice[pos + 1] == b'\n' {
            return Some(start + pos);
        }
        pos += 1;
    }
    None
}

/// Read a line from buf (everything up to \r\n, not including the \r\n).
/// Advances past the \r\n. Returns None if no complete line yet.
#[inline]
fn read_line(buf: &mut BytesMut) -> Option<BytesMut> {
    if let Some(pos) = find_crlf(buf, 0) {
        let line = buf.split_to(pos);
        buf.advance(2); // skip \r\n
        Some(line)
    } else {
        None
    }
}

/// Parse an integer from a line (used for :, array lengths, bulk string lengths, etc.)
#[inline]
fn parse_integer(line: &[u8]) -> RedisResult<i64> {
    // Fast path for common small positive numbers and simple negatives
    // Avoid str::from_utf8 + parse for the hot path
    if line.is_empty() {
        return Err(RedisError::from((
            ErrorKind::ParseError,
            "parse error",
            "Expected integer, got empty line".to_string(),
        )));
    }

    let (negative, digits) = if line[0] == b'-' {
        (true, &line[1..])
    } else if line[0] == b'+' {
        (false, &line[1..])
    } else {
        (false, line)
    };

    let mut val: i64 = 0;
    for &b in digits {
        if b < b'0' || b > b'9' {
            // Fall back to str parse for whitespace-trimmed values
            let s = std::str::from_utf8(line).map_err(|_| {
                RedisError::from((
                    ErrorKind::ParseError,
                    "parse error",
                    "Expected integer, got garbage".to_string(),
                ))
            })?;
            return s.trim().parse::<i64>().map_err(|_| {
                RedisError::from((
                    ErrorKind::ParseError,
                    "parse error",
                    "Expected integer, got garbage".to_string(),
                ))
            });
        }
        val = val
            .wrapping_mul(10)
            .wrapping_add((b - b'0') as i64);
    }

    if negative {
        val = -val;
    }
    Ok(val)
}

/// Core recursive RESP parser on BytesMut.
///
/// Returns:
/// - `Ok(Some(value))` when a complete value was parsed (buf is advanced)
/// - `Ok(None)` when more data is needed (buf is NOT modified)
/// - `Err(...)` on protocol errors
fn parse_value(buf: &mut BytesMut, depth: usize) -> RedisResult<Option<Value>> {
    if buf.is_empty() {
        return Ok(None);
    }

    // We need at least the type byte. Peek at it without consuming
    // because we might need to rewind if the message is incomplete.
    let type_byte = buf[0];

    // For array types, enforce max recursion depth
    if depth > MAX_RECURSE_DEPTH {
        return Err(RedisError::from((
            ErrorKind::ParseError,
            "parse error",
            "Maximum recursion depth exceeded".to_string(),
        )));
    }

    // Snapshot position so we can rewind on incomplete
    let snapshot_len = buf.len();

    // Consume the type byte
    buf.advance(1);

    let result = match type_byte {
        b'+' => parse_simple_string(buf),
        b'-' => parse_error(buf),
        b':' => parse_int_value(buf),
        b'$' => parse_bulk_string(buf),
        b'*' => parse_array(buf, depth),
        b'%' => parse_map(buf, depth),
        b'|' => parse_attribute(buf, depth),
        b'~' => parse_set(buf, depth),
        b'_' => parse_null(buf),
        b',' => parse_double(buf),
        b'#' => parse_boolean(buf),
        b'!' => parse_blob_error(buf),
        b'=' => parse_verbatim(buf),
        b'(' => parse_big_number(buf),
        b'>' => parse_push(buf, depth),
        _ => {
            // Restore the byte we consumed
            // Can't un-advance BytesMut easily, so this is a hard error
            return Err(RedisError::from((
                ErrorKind::ParseError,
                "parse error",
                format!("Unexpected byte: {type_byte:#x}"),
            )));
        }
    };

    match result {
        Ok(Some(val)) => Ok(Some(val)),
        Ok(None) => {
            // Incomplete — restore buffer to original state.
            // We consumed some bytes, but `split_to` wasn't called on the
            // real data (only advance on the type byte and possibly a line).
            // The simplest correct approach: we took a snapshot_len and can
            // calculate how many bytes were consumed, then un-consume them.
            //
            // BytesMut doesn't support un-advance, so we use a different
            // strategy: the Decoder wrapper saves a checkpoint.
            //
            // For the inner parse functions, we use a "peek then commit" pattern:
            // We work on the buffer directly and if we return None, the Decoder
            // wrapper handles rollback by working on a clone.
            //
            // ACTUALLY: We need to fix this. The recursive nature means we can't
            // easily rollback. The standard approach for Decoder is: the Decoder
            // saves the original BytesMut, tries to parse, and on None, restores.
            //
            // Let's use the simpler approach of working on a &[u8] cursor for
            // completeness checking, then doing the real parse with split_to
            // only when we know we have enough data.
            //
            // For now, the `_checkpoint` strategy in ValueCodec handles this.
            // But we need to not have advanced the real buffer...
            //
            // OK — revised design: parse functions check completeness on &[u8]
            // first (via the Cursor approach), then split. But that loses zero-copy.
            //
            // BEST APPROACH: Work on the BytesMut directly, but the Decoder
            // clones before calling parse_value. BytesMut clone is O(1) (it's
            // refcounted internally). If parse returns None, we discard the clone.
            // If it returns Some, we replace the original with the clone.
            //
            // This is exactly what tokio's LinesCodec and other decoders do.
            // The clone cost is negligible — it's just bumping an Arc refcount.
            //
            // The ValueCodec::decode() method handles this pattern.
            // Here we just return None and trust the caller saved a checkpoint.
            let _consumed = snapshot_len - buf.len();
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

// ── Individual type parsers ───────────────────────────────────────────
// Each returns Ok(Some(Value)) on success, Ok(None) on incomplete, Err on parse error.

#[inline]
fn parse_simple_string(buf: &mut BytesMut) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let s = std::str::from_utf8(&line).map_err(|_| {
        RedisError::from((ErrorKind::ParseError, "parse error", "Invalid UTF-8 in simple string".to_string()))
    })?;
    if s == "OK" {
        Ok(Some(Value::Okay))
    } else {
        Ok(Some(Value::SimpleString(s.to_string())))
    }
}

#[inline]
fn parse_error(buf: &mut BytesMut) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let s = std::str::from_utf8(&line).map_err(|_| {
        RedisError::from((ErrorKind::ParseError, "parse error", "Invalid UTF-8 in error".to_string()))
    })?;
    Ok(Some(Value::ServerError(err_parser(s))))
}

#[inline]
fn parse_int_value(buf: &mut BytesMut) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let val = parse_integer(&line)?;
    Ok(Some(Value::Int(val)))
}

/// Zero-copy bulk string: uses split_to + freeze to get Bytes without copying.
#[inline]
fn parse_bulk_string(buf: &mut BytesMut) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let size = parse_integer(&line)?;

    if size < 0 {
        return Ok(Some(Value::Nil));
    }

    let size = size as usize;
    // Need `size` bytes of data + 2 bytes for trailing \r\n
    if buf.len() < size + 2 {
        return Ok(None);
    }

    // Zero-copy: split_to gives us a new BytesMut owning the first `size` bytes,
    // freeze() converts to Bytes (immutable, ref-counted). No memcpy.
    let data = buf.split_to(size).freeze();
    buf.advance(2); // skip \r\n
    Ok(Some(Value::BulkString(data)))
}

fn parse_array(buf: &mut BytesMut, depth: usize) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let length = parse_integer(&line)?;

    if length < 0 {
        return Ok(Some(Value::Nil));
    }

    let length = length as usize;
    let mut items = Vec::with_capacity(length);
    for _ in 0..length {
        match parse_value(buf, depth + 1)? {
            Some(val) => items.push(val),
            None => return Ok(None),
        }
    }
    Ok(Some(Value::Array(items)))
}

fn parse_map(buf: &mut BytesMut, depth: usize) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let kv_length = parse_integer(&line)? as usize;

    let mut pairs = Vec::with_capacity(kv_length);
    for _ in 0..kv_length {
        let key = match parse_value(buf, depth + 1)? {
            Some(val) => val,
            None => return Ok(None),
        };
        let val = match parse_value(buf, depth + 1)? {
            Some(val) => val,
            None => return Ok(None),
        };
        pairs.push((key, val));
    }
    Ok(Some(Value::Map(pairs)))
}

fn parse_attribute(buf: &mut BytesMut, depth: usize) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let kv_length = parse_integer(&line)? as usize;

    // Read kv_length key-value pairs + 1 data element
    let mut attributes = Vec::with_capacity(kv_length);
    for _ in 0..kv_length {
        let key = match parse_value(buf, depth + 1)? {
            Some(val) => val,
            None => return Ok(None),
        };
        let val = match parse_value(buf, depth + 1)? {
            Some(val) => val,
            None => return Ok(None),
        };
        attributes.push((key, val));
    }
    let data = match parse_value(buf, depth + 1)? {
        Some(val) => val,
        None => return Ok(None),
    };
    Ok(Some(Value::Attribute {
        data: Box::new(data),
        attributes,
    }))
}

fn parse_set(buf: &mut BytesMut, depth: usize) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let length = parse_integer(&line)?;

    if length < 0 {
        return Ok(Some(Value::Nil));
    }

    let length = length as usize;
    let mut items = Vec::with_capacity(length);
    for _ in 0..length {
        match parse_value(buf, depth + 1)? {
            Some(val) => items.push(val),
            None => return Ok(None),
        }
    }
    Ok(Some(Value::Set(items)))
}

#[inline]
fn parse_null(buf: &mut BytesMut) -> RedisResult<Option<Value>> {
    // _\r\n
    match read_line(buf) {
        Some(_) => Ok(Some(Value::Nil)),
        None => Ok(None),
    }
}

#[inline]
fn parse_double(buf: &mut BytesMut) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let s = std::str::from_utf8(&line).map_err(|_| {
        RedisError::from((ErrorKind::ParseError, "parse error", "Invalid UTF-8 in double".to_string()))
    })?;
    let val = s.trim().parse::<f64>().map_err(|e| {
        RedisError::from((ErrorKind::ParseError, "parse error", format!("Expected double: {e}")))
    })?;
    Ok(Some(Value::Double(val)))
}

#[inline]
fn parse_boolean(buf: &mut BytesMut) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    match line.as_ref() {
        b"t" => Ok(Some(Value::Boolean(true))),
        b"f" => Ok(Some(Value::Boolean(false))),
        _ => Err(RedisError::from((
            ErrorKind::ParseError,
            "parse error",
            "Expected boolean, got garbage".to_string(),
        ))),
    }
}

/// Read a "blob" - length-prefixed binary string (used by blob errors, verbatim strings)
#[inline]
fn read_blob(buf: &mut BytesMut) -> RedisResult<Option<String>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let size = parse_integer(&line)?;

    if size < 0 {
        return Ok(Some(String::new()));
    }

    let size = size as usize;
    if buf.len() < size + 2 {
        return Ok(None);
    }

    let data = &buf[..size];
    let s = String::from_utf8_lossy(data).to_string();
    buf.advance(size + 2); // data + \r\n
    Ok(Some(s))
}

#[inline]
fn parse_blob_error(buf: &mut BytesMut) -> RedisResult<Option<Value>> {
    match read_blob(buf)? {
        Some(s) => Ok(Some(Value::ServerError(err_parser(&s)))),
        None => Ok(None),
    }
}

#[inline]
fn parse_verbatim(buf: &mut BytesMut) -> RedisResult<Option<Value>> {
    match read_blob(buf)? {
        Some(s) => {
            if let Some((format, text)) = s.split_once(':') {
                let format = match format {
                    "txt" => VerbatimFormat::Text,
                    "mkd" => VerbatimFormat::Markdown,
                    x => VerbatimFormat::Unknown(x.to_string()),
                };
                Ok(Some(Value::VerbatimString {
                    format,
                    text: text.to_string(),
                }))
            } else {
                Err(RedisError::from((
                    ErrorKind::ParseError,
                    "parse error",
                    "parse error when decoding verbatim string".to_string(),
                )))
            }
        }
        None => Ok(None),
    }
}

#[inline]
fn parse_big_number(buf: &mut BytesMut) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let val = BigInt::parse_bytes(&line, 10).ok_or_else(|| {
        RedisError::from((
            ErrorKind::ParseError,
            "parse error",
            "Expected bigint, got garbage".to_string(),
        ))
    })?;
    Ok(Some(Value::BigNumber(val)))
}

fn parse_push(buf: &mut BytesMut, depth: usize) -> RedisResult<Option<Value>> {
    let line = match read_line(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    let length = parse_integer(&line)?;

    if length <= 0 {
        return Ok(Some(Value::Push {
            kind: PushKind::Other(String::new()),
            data: vec![],
        }));
    }

    let length = length as usize;
    let mut items = Vec::with_capacity(length);
    for _ in 0..length {
        match parse_value(buf, depth + 1)? {
            Some(val) => items.push(val),
            None => return Ok(None),
        }
    }

    let mut it = items.into_iter();
    let first = it.next().unwrap_or(Value::Nil);
    if let Value::BulkString(kind) = first {
        let push_kind = String::from_utf8(kind.to_vec()).map_err(|_| {
            RedisError::from((
                ErrorKind::ParseError,
                "parse error",
                "Invalid UTF-8 in push kind".to_string(),
            ))
        })?;
        Ok(Some(Value::Push {
            kind: get_push_kind(push_kind),
            data: it.collect(),
        }))
    } else if let Value::SimpleString(kind) = first {
        Ok(Some(Value::Push {
            kind: get_push_kind(kind),
            data: it.collect(),
        }))
    } else {
        Err(RedisError::from((
            ErrorKind::ParseError,
            "parse error",
            "parse error when decoding push".to_string(),
        )))
    }
}

// ── Codec (Decoder/Encoder for tokio Framed) ─────────────────────────

mod aio_support {
    use super::*;
    use tokio::io::AsyncRead;
    use tokio_util::codec::{Decoder, Encoder};

    #[derive(Default)]
    pub struct ValueCodec;

    impl Encoder<Vec<u8>> for ValueCodec {
        type Error = RedisError;
        fn encode(&mut self, item: Vec<u8>, dst: &mut BytesMut) -> Result<(), Self::Error> {
            dst.extend_from_slice(item.as_ref());
            Ok(())
        }
    }

    impl Decoder for ValueCodec {
        type Item = RedisResult<Value>;
        type Error = RedisError;

        fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            if src.is_empty() {
                return Ok(None);
            }

            // Clone is O(1) for BytesMut — just bumps the refcount.
            // We parse on the clone; if complete, we replace src with the
            // advanced clone. If incomplete, src is untouched.
            let mut attempt = src.clone();
            match parse_value(&mut attempt, 0) {
                Ok(Some(val)) => {
                    // Commit: advance src by the amount consumed
                    let consumed = src.len() - attempt.len();
                    *src = attempt;
                    // Reserve some capacity if we're getting low,
                    // to reduce syscalls on the next read
                    let _ = consumed; // used above via split
                    Ok(Some(Ok(val)))
                }
                Ok(None) => {
                    // Incomplete — leave src untouched, ask for more data
                    Ok(None)
                }
                Err(e) => Err(e),
            }
        }

        fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            if src.is_empty() {
                return Ok(None);
            }
            self.decode(src)
        }
    }

    /// Parses a redis value asynchronously from an AsyncRead source.
    ///
    /// This is a compatibility shim — the primary fast path is through
    /// ValueCodec (Decoder) used with Framed. This function is only
    /// re-exported for API compatibility.
    pub async fn parse_redis_value_async<R>(
        _decoder: &mut (),
        read: &mut R,
    ) -> RedisResult<Value>
    where
        R: AsyncRead + std::marker::Unpin,
    {
        use tokio::io::AsyncReadExt;

        let mut buf = BytesMut::with_capacity(4096);
        loop {
            let n = read.read_buf(&mut buf).await?;
            if n == 0 {
                return Err(RedisError::from(io::Error::from(io::ErrorKind::UnexpectedEof)));
            }
            match parse_value(&mut buf, 0) {
                Ok(Some(val)) => return Ok(val),
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

pub use self::aio_support::*;

/// The internal redis response parser.
pub struct Parser;

impl Default for Parser {
    fn default() -> Self {
        Parser
    }
}

impl Parser {
    pub fn new() -> Parser {
        Parser
    }

    /// Parses synchronously into a single value from the reader.
    pub fn parse_value<T: Read>(&mut self, mut reader: T) -> RedisResult<Value> {
        let mut buf = BytesMut::with_capacity(4096);
        let mut tmp = [0u8; 4096];
        loop {
            let n = reader.read(&mut tmp)?;
            if n == 0 {
                return Err(RedisError::from(io::Error::from(io::ErrorKind::UnexpectedEof)));
            }
            buf.extend_from_slice(&tmp[..n]);

            // Try to parse after each read
            let mut attempt = buf.clone();
            match parse_value(&mut attempt, 0) {
                Ok(Some(val)) => return Ok(val),
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// Parses bytes into a redis value.
pub fn parse_redis_value(bytes: &[u8]) -> RedisResult<Value> {
    let mut buf = BytesMut::from(bytes);
    match parse_value(&mut buf, 0)? {
        Some(val) => Ok(val),
        None => Err(RedisError::from(io::Error::from(io::ErrorKind::UnexpectedEof))),
    }
}

#[cfg(test)]
mod tests {
    use crate::valkey::types::make_extension_error;

    use super::*;

    #[test]
    fn decode_eof_returns_none_at_eof() {
        use tokio_util::codec::Decoder;
        let mut codec = ValueCodec::default();

        let mut bytes = bytes::BytesMut::from(&b"+GET 123\r\n"[..]);
        assert_eq!(
            codec.decode_eof(&mut bytes),
            Ok(Some(Ok(parse_redis_value(b"+GET 123\r\n").unwrap())))
        );
        assert_eq!(codec.decode_eof(&mut bytes), Ok(None));
        assert_eq!(codec.decode_eof(&mut bytes), Ok(None));
    }

    #[test]
    fn decode_eof_returns_error_inside_array_and_can_parse_more_inputs() {
        use tokio_util::codec::Decoder;
        let mut codec = ValueCodec::default();

        let mut bytes =
            bytes::BytesMut::from(b"*3\r\n+OK\r\n-LOADING server is loading\r\n+OK\r\n".as_slice());
        let result = codec.decode_eof(&mut bytes).unwrap().unwrap();

        assert_eq!(
            result.unwrap().extract_error(),
            Err(RedisError::from((
                ErrorKind::BusyLoadingError,
                "An error was signalled by the server",
                "server is loading".to_string()
            )))
        );

        let mut bytes = bytes::BytesMut::from(b"+OK\r\n".as_slice());
        let result = codec.decode_eof(&mut bytes).unwrap().unwrap();

        assert_eq!(result, Ok(Value::Okay));
    }

    #[test]
    fn parse_nested_error_and_handle_more_inputs() {
        let bytes = b"*3\r\n+OK\r\n-LOADING server is loading\r\n+OK\r\n";
        let result = parse_redis_value(bytes);

        assert_eq!(
            result.unwrap().extract_error(),
            Err(RedisError::from((
                ErrorKind::BusyLoadingError,
                "An error was signalled by the server",
                "server is loading".to_string()
            )))
        );

        let result = parse_redis_value(b"+OK\r\n").unwrap();

        assert_eq!(result, Value::Okay);
    }

    #[test]
    fn decode_resp3_double() {
        let val = parse_redis_value(b",1.23\r\n").unwrap();
        assert_eq!(val, Value::Double(1.23));
        let val = parse_redis_value(b",nan\r\n").unwrap();
        if let Value::Double(val) = val {
            assert!(val.is_sign_positive());
            assert!(val.is_nan());
        } else {
            panic!("expected double");
        }
        // -nan is supported prior to redis 7.2
        let val = parse_redis_value(b",-nan\r\n").unwrap();
        if let Value::Double(val) = val {
            assert!(val.is_sign_negative());
            assert!(val.is_nan());
        } else {
            panic!("expected double");
        }
        //Allow doubles in scientific E notation
        let val = parse_redis_value(b",2.67923e+8\r\n").unwrap();
        assert_eq!(val, Value::Double(267923000.0));
        let val = parse_redis_value(b",2.67923E+8\r\n").unwrap();
        assert_eq!(val, Value::Double(267923000.0));
        let val = parse_redis_value(b",-2.67923E+8\r\n").unwrap();
        assert_eq!(val, Value::Double(-267923000.0));
        let val = parse_redis_value(b",2.1E-2\r\n").unwrap();
        assert_eq!(val, Value::Double(0.021));

        let val = parse_redis_value(b",-inf\r\n").unwrap();
        assert_eq!(val, Value::Double(-f64::INFINITY));
        let val = parse_redis_value(b",inf\r\n").unwrap();
        assert_eq!(val, Value::Double(f64::INFINITY));
    }

    #[test]
    fn decode_resp3_map() {
        let val = parse_redis_value(b"%2\r\n+first\r\n:1\r\n+second\r\n:2\r\n").unwrap();
        let mut v = val.as_map_iter().unwrap();
        assert_eq!(
            (&Value::SimpleString("first".to_string()), &Value::Int(1)),
            v.next().unwrap()
        );
        assert_eq!(
            (&Value::SimpleString("second".to_string()), &Value::Int(2)),
            v.next().unwrap()
        );
    }

    #[test]
    fn decode_resp3_boolean() {
        let val = parse_redis_value(b"#t\r\n").unwrap();
        assert_eq!(val, Value::Boolean(true));
        let val = parse_redis_value(b"#f\r\n").unwrap();
        assert_eq!(val, Value::Boolean(false));
        let val = parse_redis_value(b"#x\r\n");
        assert!(val.is_err());
        let val = parse_redis_value(b"#\r\n");
        assert!(val.is_err());
    }

    #[test]
    fn decode_resp3_blob_error() {
        let val = parse_redis_value(b"!21\r\nSYNTAX invalid syntax\r\n");
        assert_eq!(
            val.unwrap().extract_error().err(),
            Some(make_extension_error(
                "SYNTAX".to_string(),
                Some("invalid syntax".to_string())
            ))
        )
    }

    #[test]
    fn decode_resp3_big_number() {
        let val = parse_redis_value(b"(3492890328409238509324850943850943825024385\r\n").unwrap();
        assert_eq!(
            val,
            Value::BigNumber(
                BigInt::parse_bytes(b"3492890328409238509324850943850943825024385", 10).unwrap()
            )
        );
    }

    #[test]
    fn decode_resp3_set() {
        let val = parse_redis_value(b"~5\r\n+orange\r\n+apple\r\n#t\r\n:100\r\n:999\r\n").unwrap();
        let v = val.as_sequence().unwrap();
        assert_eq!(Value::SimpleString("orange".to_string()), v[0]);
        assert_eq!(Value::SimpleString("apple".to_string()), v[1]);
        assert_eq!(Value::Boolean(true), v[2]);
        assert_eq!(Value::Int(100), v[3]);
        assert_eq!(Value::Int(999), v[4]);
    }

    #[test]
    fn decode_resp3_push() {
        let val = parse_redis_value(b">3\r\n+message\r\n+some_channel\r\n+this is the message\r\n")
            .unwrap();
        if let Value::Push { ref kind, ref data } = val {
            assert_eq!(&PushKind::Message, kind);
            assert_eq!(Value::SimpleString("some_channel".to_string()), data[0]);
            assert_eq!(
                Value::SimpleString("this is the message".to_string()),
                data[1]
            );
        } else {
            panic!("Expected Value::Push")
        }
    }

    #[test]
    fn test_max_recursion_depth() {
        let bytes = b"*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n";
        match parse_redis_value(bytes) {
            Ok(_) => panic!("Expected Err"),
            Err(e) => assert!(matches!(e.kind(), ErrorKind::ParseError)),
        }
    }

    #[test]
    fn test_bulk_string_zero_copy() {
        // Verify that BulkString parsing produces correct Bytes
        let val = parse_redis_value(b"$5\r\nhello\r\n").unwrap();
        assert_eq!(val, Value::BulkString(bytes::Bytes::from_static(b"hello")));
    }

    #[test]
    fn test_nil_bulk_string() {
        let val = parse_redis_value(b"$-1\r\n").unwrap();
        assert_eq!(val, Value::Nil);
    }

    #[test]
    fn test_empty_bulk_string() {
        let val = parse_redis_value(b"$0\r\n\r\n").unwrap();
        assert_eq!(val, Value::BulkString(bytes::Bytes::new()));
    }

    #[test]
    fn test_simple_integer() {
        let val = parse_redis_value(b":42\r\n").unwrap();
        assert_eq!(val, Value::Int(42));
        let val = parse_redis_value(b":-100\r\n").unwrap();
        assert_eq!(val, Value::Int(-100));
        let val = parse_redis_value(b":0\r\n").unwrap();
        assert_eq!(val, Value::Int(0));
    }

    #[test]
    fn test_incomplete_returns_error_for_parse_redis_value() {
        // parse_redis_value on incomplete data should error (not hang)
        let result = parse_redis_value(b"$5\r\nhel");
        assert!(result.is_err());
    }

    #[test]
    fn test_codec_incomplete_returns_none() {
        use tokio_util::codec::Decoder;
        let mut codec = ValueCodec::default();

        // Incomplete bulk string — should return None, not error
        let mut bytes = bytes::BytesMut::from(&b"$5\r\nhel"[..]);
        assert_eq!(codec.decode(&mut bytes), Ok(None));
        // Buffer should be untouched
        assert_eq!(&bytes[..], b"$5\r\nhel");

        // Now complete it
        bytes.extend_from_slice(b"lo\r\n");
        let result = codec.decode(&mut bytes).unwrap().unwrap();
        assert_eq!(result, Ok(Value::BulkString(bytes::Bytes::from_static(b"hello"))));
    }
}
