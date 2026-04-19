use std::io::{self, Read};

use crate::value::{
    ErrorKind, PushKind, Error, Result, Value, VerbatimFormat, make_extension_error,
};

use bytes::{Buf, BytesMut};
use num_bigint::BigInt;

const MAX_RECURSE_DEPTH: usize = 100;
const MAX_BULK_STRING_BYTES: usize = 512 * 1024 * 1024; // 512 MiB

/// Shorthand for ParseError construction. Reduces 4-line error sites to 1 line.
#[inline]
fn parse_err(msg: impl Into<String>) -> Error {
    Error::from((ErrorKind::ParseError, "parse error", msg.into()))
}

fn err_parser(line: &str) -> Error {
    let mut pieces = line.splitn(2, ' ');
    let kind = match pieces.next().unwrap() {
        "ERR" => ErrorKind::ResponseError,
        "EXECABORT" => ErrorKind::ExecAbortError,
        "LOADING" => ErrorKind::BusyLoadingError,
        "NOSCRIPT" => ErrorKind::NoScriptError,
        "MOVED" => {
            tracing::debug!(
                target: "ferriskey",
                event = "moved_redirect",
                "ferriskey: MOVED redirect from server"
            );
            ErrorKind::Moved
        }
        "ASK" => ErrorKind::Ask,
        "TRYAGAIN" => ErrorKind::TryAgain,
        "CLUSTERDOWN" => ErrorKind::ClusterDown,
        "CROSSSLOT" => ErrorKind::CrossSlot,
        "MASTERDOWN" => ErrorKind::MasterDown,
        "READONLY" => ErrorKind::ReadOnly,
        "NOTBUSY" => ErrorKind::NotBusy,
        "NOPERM" => ErrorKind::PermissionDenied,
        code => {
            return make_extension_error(
                code.to_string(),
                pieces.next().map(|str| str.to_string()),
            );
        }
    };
    let detail = pieces.next().map(|str| str.to_string());
    match detail {
        Some(d) => Error::from((kind, "server error", d)),
        None => Error::from((kind, "server error")),
    }
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

// ── Two-pass RESP parser ──────────────────────────────────────────────
//
// Pass 1 (scan): Read-only walk on &[u8] to check if a complete RESP
//   message exists. Returns the total byte count consumed, or None if
//   incomplete. No allocations, no mutations.
//
// Pass 2 (parse): Only called when scan confirms completeness. Operates
//   on &mut BytesMut with split_to() for zero-copy BulkString extraction.
//   Never returns None (the message is known to be complete).
//
// This avoids the O(n) BytesMut::clone() that the single-pass approach
// required for rollback on incomplete messages.

// ── Pass 1: Scan ─────────────────────────────────────────────────────

/// Find \r\n starting at `pos` in `buf`. Returns offset of '\r', or None.
#[inline]
fn find_crlf(buf: &[u8], pos: usize) -> Option<usize> {
    let slice = &buf[pos..];
    let mut i = 0;
    while i < slice.len().saturating_sub(1) {
        if slice[i] == b'\r' && slice[i + 1] == b'\n' {
            return Some(pos + i);
        }
        i += 1;
    }
    None
}

/// Parse an integer from a byte slice (no allocations).
#[inline]
fn parse_integer(line: &[u8]) -> Result<i64> {
    if line.is_empty() {
        return Err(parse_err("Expected integer, got empty line".to_string()));
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
        if !b.is_ascii_digit() {
            let s = std::str::from_utf8(line)
                .map_err(|_| parse_err("Expected integer, got garbage".to_string()))?;
            return s
                .trim()
                .parse::<i64>()
                .map_err(|_| parse_err("Expected integer, got garbage".to_string()));
        }
        val = val
            .checked_mul(10)
            .and_then(|v| v.checked_add((b - b'0') as i64))
            .ok_or_else(|| parse_err("Integer overflow in RESP integer".to_string()))?;
    }

    if negative {
        val = -val;
    }
    Ok(val)
}

/// Scan a single RESP value starting at `pos` in `buf`.
/// Returns the byte position AFTER the value if complete, or None if incomplete.
/// Returns Err on protocol errors (bad type byte, exceeds recursion depth).
fn scan_value(buf: &[u8], pos: usize, depth: usize) -> Result<Option<usize>> {
    if pos >= buf.len() {
        return Ok(None);
    }

    if depth > MAX_RECURSE_DEPTH {
        return Err(parse_err("Maximum recursion depth exceeded".to_string()));
    }

    let type_byte = buf[pos];
    let after_type = pos + 1;

    match type_byte {
        // Simple types: read until \r\n
        b'+' | b'-' | b':' | b'_' | b',' | b'#' | b'(' => scan_line(buf, after_type),

        // Bulk string: $<len>\r\n<data>\r\n
        b'$' => scan_bulk(buf, after_type),

        // Blob error / verbatim: same framing as bulk string
        b'!' | b'=' => scan_bulk(buf, after_type),

        // Array / Set / Push: *<count>\r\n<elements...>
        b'*' | b'~' | b'>' => scan_aggregate(buf, after_type, depth),

        // Map: %<count>\r\n<key><val>... (count = number of pairs)
        b'%' => scan_map(buf, after_type, depth),

        // Attribute: |<count>\r\n<key><val>...<data>
        b'|' => scan_attribute(buf, after_type, depth),

        _ => Err(parse_err(format!("Unexpected byte: {type_byte:#x}"))),
    }
}

/// Scan past a \r\n-terminated line. Returns position after the \r\n.
#[inline]
fn scan_line(buf: &[u8], pos: usize) -> Result<Option<usize>> {
    match find_crlf(buf, pos) {
        Some(cr) => Ok(Some(cr + 2)),
        None => Ok(None),
    }
}

/// Scan a bulk string/blob: <len>\r\n<data>\r\n
#[inline]
fn scan_bulk(buf: &[u8], pos: usize) -> Result<Option<usize>> {
    let cr = match find_crlf(buf, pos) {
        Some(cr) => cr,
        None => return Ok(None),
    };
    let size = parse_integer(&buf[pos..cr])?;
    if size < 0 {
        // Null bulk string: just the length line
        return Ok(Some(cr + 2));
    }
    let size = size as usize;
    if size > MAX_BULK_STRING_BYTES {
        return Err(parse_err(format!(
            "Bulk string length {size} exceeds maximum of {MAX_BULK_STRING_BYTES} bytes"
        )));
    }
    let data_start = cr + 2;
    let end = data_start + size + 2; // data + \r\n
    if buf.len() < end {
        Ok(None)
    } else {
        Ok(Some(end))
    }
}

/// Scan an aggregate type (array, set, push): <count>\r\n<elements...>
fn scan_aggregate(buf: &[u8], pos: usize, depth: usize) -> Result<Option<usize>> {
    let cr = match find_crlf(buf, pos) {
        Some(cr) => cr,
        None => return Ok(None),
    };
    let count = parse_integer(&buf[pos..cr])?;
    if count < 0 {
        return Ok(Some(cr + 2));
    }
    let count = count as usize;
    let mut cursor = cr + 2;
    for _ in 0..count {
        match scan_value(buf, cursor, depth + 1)? {
            Some(next) => cursor = next,
            None => return Ok(None),
        }
    }
    Ok(Some(cursor))
}

/// Scan a map: <count>\r\n<key><val>... (count pairs = 2*count elements)
fn scan_map(buf: &[u8], pos: usize, depth: usize) -> Result<Option<usize>> {
    let cr = match find_crlf(buf, pos) {
        Some(cr) => cr,
        None => return Ok(None),
    };
    let count = parse_integer(&buf[pos..cr])?;
    if count < 0 {
        return Ok(Some(cr + 2));
    }
    let count = count as usize;
    let mut cursor = cr + 2;
    for _ in 0..count * 2 {
        match scan_value(buf, cursor, depth + 1)? {
            Some(next) => cursor = next,
            None => return Ok(None),
        }
    }
    Ok(Some(cursor))
}

/// Scan an attribute: <count>\r\n<key><val>...<data> (count pairs + 1 data element)
fn scan_attribute(buf: &[u8], pos: usize, depth: usize) -> Result<Option<usize>> {
    let cr = match find_crlf(buf, pos) {
        Some(cr) => cr,
        None => return Ok(None),
    };
    let count = parse_integer(&buf[pos..cr])?;
    if count < 0 {
        return Ok(Some(cr + 2));
    }
    let count = count as usize;
    let mut cursor = cr + 2;
    // count key-value pairs + 1 data element = count*2 + 1
    for _ in 0..count * 2 + 1 {
        match scan_value(buf, cursor, depth + 1)? {
            Some(next) => cursor = next,
            None => return Ok(None),
        }
    }
    Ok(Some(cursor))
}

// ── Pass 2: Parse ────────────────────────────────────────────────────
// Only called when scan confirms the buffer contains a complete message.
// parse_value_unchecked never returns None — panics are bugs.

/// Read a line from BytesMut, advancing past \r\n.
/// Returns an error if the expected \r\n terminator is not found.
#[inline]
fn read_line_unchecked(buf: &mut BytesMut) -> Result<BytesMut> {
    let pos = find_crlf(buf, 0)
        .ok_or_else(|| parse_err("Incomplete RESP line after scan".to_string()))?;
    let line = buf.split_to(pos);
    buf.advance(2); // \r\n
    Ok(line)
}

/// Parse a complete RESP value from BytesMut. Never returns None.
fn parse_value_unchecked(buf: &mut BytesMut, depth: usize) -> Result<Value> {
    let type_byte = buf[0];
    buf.advance(1);

    match type_byte {
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
        _ => Err(parse_err(format!("Unexpected byte: {type_byte:#x}"))),
    }
}

// ── Individual type parsers (pass 2, unchecked) ──────────────────────

#[inline]
fn parse_simple_string(buf: &mut BytesMut) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let s = std::str::from_utf8(&line)
        .map_err(|_| parse_err("Invalid UTF-8 in simple string".to_string()))?;
    if s == "OK" {
        Ok(Value::Okay)
    } else {
        Ok(Value::SimpleString(s.to_string()))
    }
}

#[inline]
fn parse_error(buf: &mut BytesMut) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let s =
        std::str::from_utf8(&line).map_err(|_| parse_err("Invalid UTF-8 in error".to_string()))?;
    Err(err_parser(s))
}

#[inline]
fn parse_int_value(buf: &mut BytesMut) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let val = parse_integer(&line)?;
    Ok(Value::Int(val))
}

/// Zero-copy bulk string: split_to + freeze. No memcpy.
#[inline]
fn parse_bulk_string(buf: &mut BytesMut) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let size = parse_integer(&line)?;
    if size < 0 {
        return Ok(Value::Nil);
    }
    let size = size as usize;
    let data = buf.split_to(size).freeze();
    buf.advance(2); // \r\n
    Ok(Value::BulkString(data))
}

/// Maximum array/map/set length to prevent DoS from crafted responses.
const MAX_COLLECTION_LENGTH: usize = 1_000_000;

fn parse_array(buf: &mut BytesMut, depth: usize) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let length = parse_integer(&line)?;
    if length < 0 {
        return Ok(Value::Nil);
    }
    let length = length as usize;
    if length > MAX_COLLECTION_LENGTH {
        return Err((
            ErrorKind::ParseError,
            "Array length exceeds maximum",
            format!("got {length}, max is {MAX_COLLECTION_LENGTH}"),
        )
            .into());
    }
    let mut items = Vec::with_capacity(length);
    for _ in 0..length {
        // Array elements can be errors (e.g. EXEC responses with per-command failures).
        // Top-level errors return Err directly; array element errors are Err in the Vec.
        items.push(parse_value_unchecked(buf, depth + 1));
    }
    Ok(Value::Array(items))
}

fn parse_map(buf: &mut BytesMut, depth: usize) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let kv_length = parse_integer(&line)?;
    if kv_length < 0 {
        return Ok(Value::Nil);
    }
    let kv_length = kv_length as usize;
    if kv_length > MAX_COLLECTION_LENGTH {
        return Err((
            ErrorKind::ParseError,
            "Map length exceeds maximum",
            format!("got {kv_length}, max is {MAX_COLLECTION_LENGTH}"),
        )
            .into());
    }
    let mut pairs = Vec::with_capacity(kv_length);
    for _ in 0..kv_length {
        let key = parse_value_unchecked(buf, depth + 1)?;
        let val = parse_value_unchecked(buf, depth + 1)?;
        pairs.push((key, val));
    }
    Ok(Value::Map(pairs))
}

fn parse_attribute(buf: &mut BytesMut, depth: usize) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let kv_length = parse_integer(&line)?;
    if kv_length < 0 {
        return Ok(Value::Nil);
    }
    let kv_length = kv_length as usize;
    if kv_length > MAX_COLLECTION_LENGTH {
        return Err((ErrorKind::ParseError, "Attribute length exceeds maximum",
            format!("got {kv_length}, max is {MAX_COLLECTION_LENGTH}")).into());
    }
    let mut attributes = Vec::with_capacity(kv_length);
    for _ in 0..kv_length {
        let key = parse_value_unchecked(buf, depth + 1)?;
        let val = parse_value_unchecked(buf, depth + 1)?;
        attributes.push((key, val));
    }
    let data = parse_value_unchecked(buf, depth + 1)?;
    Ok(Value::Attribute {
        data: Box::new(data),
        attributes,
    })
}

fn parse_set(buf: &mut BytesMut, depth: usize) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let length = parse_integer(&line)?;
    if length < 0 {
        return Ok(Value::Nil);
    }
    let length = length as usize;
    if length > MAX_COLLECTION_LENGTH {
        return Err((ErrorKind::ParseError, "Set length exceeds maximum",
            format!("got {length}, max is {MAX_COLLECTION_LENGTH}")).into());
    }
    let mut items = Vec::with_capacity(length);
    for _ in 0..length {
        items.push(parse_value_unchecked(buf, depth + 1)?);
    }
    Ok(Value::Set(items))
}

#[inline]
fn parse_null(buf: &mut BytesMut) -> Result<Value> {
    let _line = read_line_unchecked(buf)?;
    Ok(Value::Nil)
}

#[inline]
fn parse_double(buf: &mut BytesMut) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let s =
        std::str::from_utf8(&line).map_err(|_| parse_err("Invalid UTF-8 in double".to_string()))?;
    let val = s
        .trim()
        .parse::<f64>()
        .map_err(|e| parse_err(format!("Expected double: {e}")))?;
    Ok(Value::Double(val))
}

#[inline]
fn parse_boolean(buf: &mut BytesMut) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    match line.as_ref() {
        b"t" => Ok(Value::Boolean(true)),
        b"f" => Ok(Value::Boolean(false)),
        _ => Err(parse_err("Expected boolean, got garbage".to_string())),
    }
}

/// Read a blob (length-prefixed string) for blob errors and verbatim strings.
#[inline]
fn read_blob_unchecked(buf: &mut BytesMut) -> Result<String> {
    let line = read_line_unchecked(buf)?;
    let size = parse_integer(&line)?;
    if size < 0 {
        return Ok(String::new());
    }
    let size = size as usize;
    let data = &buf[..size];
    let s = std::str::from_utf8(data)
        .map_err(|_| parse_err("Invalid UTF-8 in blob string".to_string()))?
        .to_string();
    buf.advance(size + 2);
    Ok(s)
}

#[inline]
fn parse_blob_error(buf: &mut BytesMut) -> Result<Value> {
    let s = read_blob_unchecked(buf)?;
    Err(err_parser(&s))
}

#[inline]
fn parse_verbatim(buf: &mut BytesMut) -> Result<Value> {
    let s = read_blob_unchecked(buf)?;
    if let Some((format, text)) = s.split_once(':') {
        let format = match format {
            "txt" => VerbatimFormat::Text,
            "mkd" => VerbatimFormat::Markdown,
            x => VerbatimFormat::Unknown(x.to_string()),
        };
        Ok(Value::VerbatimString {
            format,
            text: text.to_string(),
        })
    } else {
        Err(parse_err(
            "Parse error when decoding verbatim string".to_string(),
        ))
    }
}

#[inline]
fn parse_big_number(buf: &mut BytesMut) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let val = BigInt::parse_bytes(&line, 10)
        .ok_or_else(|| parse_err("Expected bigint, got garbage".to_string()))?;
    Ok(Value::BigNumber(val))
}

fn parse_push(buf: &mut BytesMut, depth: usize) -> Result<Value> {
    let line = read_line_unchecked(buf)?;
    let length = parse_integer(&line)?;

    if length <= 0 {
        return Ok(Value::Push {
            kind: PushKind::Other(String::new()),
            data: vec![],
        });
    }

    let length = length as usize;
    if length > MAX_COLLECTION_LENGTH {
        return Err((ErrorKind::ParseError, "Push length exceeds maximum",
            format!("got {length}, max is {MAX_COLLECTION_LENGTH}")).into());
    }
    let mut items = Vec::with_capacity(length);
    for _ in 0..length {
        items.push(parse_value_unchecked(buf, depth + 1)?);
    }

    let mut it = items.into_iter();
    let first = it.next().unwrap_or(Value::Nil);
    if let Value::BulkString(kind) = first {
        let push_kind = String::from_utf8(kind.to_vec())
            .map_err(|_| parse_err("Invalid UTF-8 in push kind".to_string()))?;
        Ok(Value::Push {
            kind: get_push_kind(push_kind),
            data: it.collect(),
        })
    } else if let Value::SimpleString(kind) = first {
        Ok(Value::Push {
            kind: get_push_kind(kind),
            data: it.collect(),
        })
    } else {
        Err(parse_err("Parse error when decoding push".to_string()))
    }
}

// ── Codec (Decoder/Encoder for tokio Framed) ─────────────────────────

mod aio_support {
    use super::*;
    use tokio::io::AsyncRead;
    use tokio_util::codec::{Decoder, Encoder};

    #[derive(Default)]
    pub struct ValueCodec;

    impl Encoder<bytes::Bytes> for ValueCodec {
        type Error = Error;
        fn encode(&mut self, item: bytes::Bytes, dst: &mut BytesMut) -> std::result::Result<(), Self::Error> {
            dst.extend_from_slice(&item);
            Ok(())
        }
    }

    impl Decoder for ValueCodec {
        type Item = Result<Value>;
        type Error = Error;

        fn decode(&mut self, src: &mut BytesMut) -> std::result::Result<Option<Self::Item>, Self::Error> {
            if src.is_empty() {
                return Ok(None);
            }

            // Pass 1: Scan on &[u8] — read-only, no allocations, no mutations.
            // Returns the byte position after the complete message, or None.
            match scan_value(src, 0, 0)? {
                None => Ok(None), // Incomplete — src untouched
                Some(_end) => {
                    // Pass 2: Parse on &mut BytesMut — uses split_to() for
                    // zero-copy BulkString. Never returns None since scan
                    // confirmed completeness.
                    //
                    // IMPORTANT: Server error responses (-ERR, -WRONGTYPE, etc.)
                    // are returned as Ok(Err(e)) NOT Err(e). If we return Err(e)
                    // from Decoder::decode(), tokio-util's Framed sets has_errored=true
                    // and returns None (fake EOF) on the next poll, closing the stream.
                    // Server errors are application-level, not codec-level failures.
                    let val = parse_value_unchecked(src, 0);
                    Ok(Some(val))
                }
            }
        }

        fn decode_eof(&mut self, src: &mut BytesMut) -> std::result::Result<Option<Self::Item>, Self::Error> {
            if src.is_empty() {
                return Ok(None);
            }
            self.decode(src)
        }
    }

    /// Parses a valkey value asynchronously from an AsyncRead source.
    #[allow(dead_code)]
    pub async fn parse_valkey_value_async<R>(_decoder: &mut (), read: &mut R) -> Result<Value>
    where
        R: AsyncRead + std::marker::Unpin,
    {
        use tokio::io::AsyncReadExt;

        let mut buf = BytesMut::with_capacity(4096);
        loop {
            let n = read.read_buf(&mut buf).await?;
            if n == 0 {
                return Err(Error::from(io::Error::from(
                    io::ErrorKind::UnexpectedEof,
                )));
            }
            match scan_value(&buf, 0, 0) {
                Ok(Some(_)) => {
                    return parse_value_unchecked(&mut buf, 0);
                }
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

pub use self::aio_support::*;

/// The internal valkey response parser.
#[allow(dead_code)]
pub struct Parser;

impl Default for Parser {
    fn default() -> Self {
        Parser
    }
}

#[allow(dead_code)]
impl Parser {
    pub fn new() -> Parser {
        Parser
    }

    /// Parses synchronously into a single value from the reader.
    pub fn parse_value<T: Read>(&mut self, mut reader: T) -> Result<Value> {
        let mut buf = BytesMut::with_capacity(4096);
        let mut tmp = [0u8; 4096];
        loop {
            let n = reader.read(&mut tmp)?;
            if n == 0 {
                return Err(Error::from(io::Error::from(
                    io::ErrorKind::UnexpectedEof,
                )));
            }
            buf.extend_from_slice(&tmp[..n]);

            match scan_value(&buf, 0, 0) {
                Ok(Some(_)) => {
                    return parse_value_unchecked(&mut buf, 0);
                }
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// Parses bytes into a valkey value. Used in tests.
#[allow(dead_code)]
pub fn parse_valkey_value(bytes: &[u8]) -> Result<Value> {
    // For complete buffers we can skip scan — parse_value_unchecked will
    // work if the data is complete, and we get an error/panic if not.
    // But to match the old semantics (return error on incomplete), do
    // the scan first.
    match scan_value(bytes, 0, 0)? {
        Some(_) => {
            let mut buf = BytesMut::from(bytes);
            parse_value_unchecked(&mut buf, 0)
        }
        None => Err(Error::from(io::Error::from(
            io::ErrorKind::UnexpectedEof,
        ))),
    }
}

#[cfg(test)]
mod tests {
    use crate::value::make_extension_error;

    use super::*;

    #[test]
    fn decode_eof_returns_none_at_eof() {
        use tokio_util::codec::Decoder;
        let mut codec = ValueCodec;

        let mut bytes = bytes::BytesMut::from(&b"+GET 123\r\n"[..]);
        assert_eq!(
            codec.decode_eof(&mut bytes),
            Ok(Some(Ok(parse_valkey_value(b"+GET 123\r\n").unwrap())))
        );
        assert_eq!(codec.decode_eof(&mut bytes), Ok(None));
        assert_eq!(codec.decode_eof(&mut bytes), Ok(None));
    }

    #[test]
    fn decode_eof_returns_error_inside_array_and_can_parse_more_inputs() {
        use tokio_util::codec::Decoder;
        let mut codec = ValueCodec;

        let mut bytes =
            bytes::BytesMut::from(b"*3\r\n+OK\r\n-LOADING server is loading\r\n+OK\r\n".as_slice());
        let result = codec.decode_eof(&mut bytes).unwrap().unwrap();

        assert_eq!(
            result.unwrap().extract_error(),
            Err(Error::from((
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
        let result = parse_valkey_value(bytes);

        assert_eq!(
            result.unwrap().extract_error(),
            Err(Error::from((
                ErrorKind::BusyLoadingError,
                "An error was signalled by the server",
                "server is loading".to_string()
            )))
        );

        let result = parse_valkey_value(b"+OK\r\n").unwrap();

        assert_eq!(result, Value::Okay);
    }

    #[test]
    fn decode_resp3_double() {
        let val = parse_valkey_value(b",1.23\r\n").unwrap();
        assert_eq!(val, Value::Double(1.23));
        let val = parse_valkey_value(b",nan\r\n").unwrap();
        if let Value::Double(val) = val {
            assert!(val.is_sign_positive());
            assert!(val.is_nan());
        } else {
            panic!("expected double");
        }
        let val = parse_valkey_value(b",-nan\r\n").unwrap();
        if let Value::Double(val) = val {
            assert!(val.is_sign_negative());
            assert!(val.is_nan());
        } else {
            panic!("expected double");
        }
        let val = parse_valkey_value(b",2.67923e+8\r\n").unwrap();
        assert_eq!(val, Value::Double(267923000.0));
        let val = parse_valkey_value(b",2.67923E+8\r\n").unwrap();
        assert_eq!(val, Value::Double(267923000.0));
        let val = parse_valkey_value(b",-2.67923E+8\r\n").unwrap();
        assert_eq!(val, Value::Double(-267923000.0));
        let val = parse_valkey_value(b",2.1E-2\r\n").unwrap();
        assert_eq!(val, Value::Double(0.021));

        let val = parse_valkey_value(b",-inf\r\n").unwrap();
        assert_eq!(val, Value::Double(-f64::INFINITY));
        let val = parse_valkey_value(b",inf\r\n").unwrap();
        assert_eq!(val, Value::Double(f64::INFINITY));
    }

    #[test]
    fn decode_resp3_map() {
        let val = parse_valkey_value(b"%2\r\n+first\r\n:1\r\n+second\r\n:2\r\n").unwrap();
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
        let val = parse_valkey_value(b"#t\r\n").unwrap();
        assert_eq!(val, Value::Boolean(true));
        let val = parse_valkey_value(b"#f\r\n").unwrap();
        assert_eq!(val, Value::Boolean(false));
        let val = parse_valkey_value(b"#x\r\n");
        assert!(val.is_err());
        let val = parse_valkey_value(b"#\r\n");
        assert!(val.is_err());
    }

    #[test]
    fn decode_resp3_blob_error() {
        // Blob errors (RESP3 !) are now returned as Err(Error) directly
        let val = parse_valkey_value(b"!21\r\nSYNTAX invalid syntax\r\n");
        assert_eq!(
            val.unwrap_err(),
            make_extension_error("SYNTAX".to_string(), Some("invalid syntax".to_string()))
        )
    }

    #[test]
    fn decode_resp3_big_number() {
        let val = parse_valkey_value(b"(3492890328409238509324850943850943825024385\r\n").unwrap();
        assert_eq!(
            val,
            Value::BigNumber(
                BigInt::parse_bytes(b"3492890328409238509324850943850943825024385", 10).unwrap()
            )
        );
    }

    #[test]
    fn decode_resp3_set() {
        let val = parse_valkey_value(b"~5\r\n+orange\r\n+apple\r\n#t\r\n:100\r\n:999\r\n").unwrap();
        let v = val.as_plain_sequence().unwrap();
        assert_eq!(Value::SimpleString("orange".to_string()), v[0]);
        assert_eq!(Value::SimpleString("apple".to_string()), v[1]);
        assert_eq!(Value::Boolean(true), v[2]);
        assert_eq!(Value::Int(100), v[3]);
        assert_eq!(Value::Int(999), v[4]);
    }

    #[test]
    fn decode_resp3_push() {
        let val =
            parse_valkey_value(b">3\r\n+message\r\n+some_channel\r\n+this is the message\r\n")
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
        let bytes = b"*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n";
        match parse_valkey_value(bytes) {
            Ok(_) => panic!("Expected Err"),
            Err(e) => assert!(matches!(e.kind(), ErrorKind::ParseError)),
        }
    }

    #[test]
    fn test_bulk_string_zero_copy() {
        let val = parse_valkey_value(b"$5\r\nhello\r\n").unwrap();
        assert_eq!(val, Value::BulkString(bytes::Bytes::from_static(b"hello")));
    }

    #[test]
    fn test_nil_bulk_string() {
        let val = parse_valkey_value(b"$-1\r\n").unwrap();
        assert_eq!(val, Value::Nil);
    }

    #[test]
    fn test_empty_bulk_string() {
        let val = parse_valkey_value(b"$0\r\n\r\n").unwrap();
        assert_eq!(val, Value::BulkString(bytes::Bytes::new()));
    }

    #[test]
    fn test_simple_integer() {
        let val = parse_valkey_value(b":42\r\n").unwrap();
        assert_eq!(val, Value::Int(42));
        let val = parse_valkey_value(b":-100\r\n").unwrap();
        assert_eq!(val, Value::Int(-100));
        let val = parse_valkey_value(b":0\r\n").unwrap();
        assert_eq!(val, Value::Int(0));
    }

    #[test]
    fn test_incomplete_returns_error_for_parse_valkey_value() {
        let result = parse_valkey_value(b"$5\r\nhel");
        assert!(result.is_err());
    }

    #[test]
    fn test_codec_incomplete_returns_none() {
        use tokio_util::codec::Decoder;
        let mut codec = ValueCodec;

        // Incomplete bulk string — should return None, not error
        let mut bytes = bytes::BytesMut::from(&b"$5\r\nhel"[..]);
        assert_eq!(codec.decode(&mut bytes), Ok(None));
        // Buffer should be untouched (two-pass: scan says incomplete, no mutation)
        assert_eq!(&bytes[..], b"$5\r\nhel");

        // Now complete it
        bytes.extend_from_slice(b"lo\r\n");
        let result = codec.decode(&mut bytes).unwrap().unwrap();
        assert_eq!(
            result,
            Ok(Value::BulkString(bytes::Bytes::from_static(b"hello")))
        );
    }

    #[test]
    fn test_scan_completeness() {
        // Complete simple string
        assert!(scan_value(b"+OK\r\n", 0, 0).unwrap().is_some());
        // Incomplete simple string
        assert!(scan_value(b"+OK", 0, 0).unwrap().is_none());
        // Complete array
        assert!(scan_value(b"*2\r\n+OK\r\n:1\r\n", 0, 0).unwrap().is_some());
        // Incomplete array (missing second element)
        assert!(scan_value(b"*2\r\n+OK\r\n", 0, 0).unwrap().is_none());
        // Complete bulk string
        assert!(scan_value(b"$3\r\nfoo\r\n", 0, 0).unwrap().is_some());
        // Incomplete bulk string (data too short)
        assert!(scan_value(b"$3\r\nfo", 0, 0).unwrap().is_none());
    }

    #[test]
    fn test_integer_overflow_rejected() {
        // Overflow i64 — should error, not wrap silently
        let result = parse_valkey_value(b":99999999999999999999\r\n");
        assert!(result.is_err());
        // Bulk string with overflowing length
        let result = parse_valkey_value(b"$99999999999999999999\r\nhello\r\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_bulk_string_size_cap() {
        // Bulk string at exactly the cap should be fine (if data were present)
        // But we test the scan rejection for oversized
        let huge = format!("${}\r\n", MAX_BULK_STRING_BYTES + 1);
        let result = parse_valkey_value(huge.as_bytes());
        assert!(result.is_err());
    }

    #[test]
    fn test_negative_map_count() {
        // Negative map count should return Nil, not panic
        let val = parse_valkey_value(b"%-1\r\n").unwrap();
        assert_eq!(val, Value::Nil);
    }

    #[test]
    fn test_negative_attribute_count() {
        // Negative attribute count should return Nil, not panic
        // Attribute with -1 count: |<count>\r\n followed by a data element
        // With negative count, we skip pairs and read the "data" element
        // But our guard returns Nil before reaching data
        let val = parse_valkey_value(b"|-1\r\n").unwrap();
        assert_eq!(val, Value::Nil);
    }

    #[test]
    fn test_empty_bulk_string_and_nil() {
        // Empty bulk string ($0)
        let val = parse_valkey_value(b"$0\r\n\r\n").unwrap();
        assert_eq!(val, Value::BulkString(bytes::Bytes::new()));
        // Nil bulk string ($-1)
        let val = parse_valkey_value(b"$-1\r\n").unwrap();
        assert_eq!(val, Value::Nil);
        // Nil array (*-1)
        let val = parse_valkey_value(b"*-1\r\n").unwrap();
        assert_eq!(val, Value::Nil);
    }
}
