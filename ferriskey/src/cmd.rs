use futures_util::{
    Stream, StreamExt,
    future::BoxFuture,
    task::{Context, Poll},
};
use std::pin::Pin;
use std::{borrow::Borrow, fmt, io};

use crate::pipeline::Pipeline;
use crate::value::{
    FromValue, Result, ToArgs, Write, from_owned_value,
};
use telemetrylib::FerrisKeySpan;

/// An argument to a valkey command
#[derive(Clone)]
pub(crate) enum Arg<D> {
    /// A normal argument
    Simple(D),
    /// A cursor position marker used in SCAN-family command argument lists.
    /// Populated by cursor_arg(); kept for exhaustive match coverage in iterators.
    #[allow(dead_code)]
    Cursor,
}

/// Represents valkey commands.
#[derive(Clone)]
pub struct Cmd {
    data: Vec<u8>,
    // Arg::Simple contains the offset that marks the end of the argument
    args: Vec<Arg<usize>>,
    cursor: Option<u64>,
    /// The span associated with this command
    span: Option<FerrisKeySpan>,
    //  A flag indicating whether this is a fenced command  (will have PING appended to ensure ordering)
    is_fenced: bool,
    /// Inflight slot tracker. When set, the slot is released when the last
    /// clone of this Cmd (or its Arc) is dropped. Used to decouple user-facing
    /// timeout from internal pipeline cleanup.
    inflight_tracker: Option<crate::value::InflightRequestTracker>,
}

/// The PING command used to fence other commands for ordering guarantees
const FENCE_COMMAND: &[u8] = b"*1\r\n$4\r\nPING\r\n";

use crate::connection::ConnectionLike as AsyncConnection;

/// The inner future of AsyncIter
struct AsyncIterInner<'a, T: FromValue + 'a, C: AsyncConnection + Send + 'a> {
    batch: std::vec::IntoIter<T>,
    con: &'a mut C,
    cmd: Cmd,
}

/// Represents the state of AsyncIter
enum IterOrFuture<'a, T: FromValue + 'a, C: AsyncConnection + Send + 'a> {
    Iter(AsyncIterInner<'a, T, C>),
    Future(BoxFuture<'a, (AsyncIterInner<'a, T, C>, Option<T>)>),
    Empty,
}

/// Represents a valkey iterator that can be used with async connections.
pub struct AsyncIter<'a, T: FromValue + 'a, C: AsyncConnection + Send + 'a> {
    inner: IterOrFuture<'a, T, C>,
}

impl<'a, T: FromValue + 'a, C: AsyncConnection + Send + 'a> AsyncIterInner<'a, T, C> {
    #[inline]
    pub async fn next_item(&mut self) -> Option<T> {
        loop {
            if let Some(v) = self.batch.next() {
                return Some(v);
            };
            if let Some(cursor) = self.cmd.cursor {
                if cursor == 0 {
                    return None;
                }
            } else {
                return None;
            }

            let rv = self.con.req_packed_command(&self.cmd).await.ok()?;
            let (cur, batch): (u64, Vec<T>) = from_owned_value(rv).ok()?;

            self.cmd.cursor = Some(cur);
            self.batch = batch.into_iter();
        }
    }
}

impl<'a, T: FromValue + 'a + Unpin + Send, C: AsyncConnection + Send + Unpin + 'a>
    AsyncIter<'a, T, C>
{
    /// ```rust,ignore
    /// # use ferriskey::AsyncCommands;
    /// # async fn scan_set() -> ferriskey::Result<()> {
    /// # let client = ferriskey::Client::open("redis://127.0.0.1/")?;
    /// # let mut con = client.get_async_connection(None).await?;
    /// con.sadd::<_, _, ()>("my_set", 42i32).await?;
    /// con.sadd::<_, _, ()>("my_set", 43i32).await?;
    /// let mut iter: ferriskey::AsyncIter<i32> = con.sscan("my_set").await?;
    /// while let Some(element) = iter.next_item().await {
    ///     assert!(element == 42 || element == 43);
    /// }
    /// # Ok(())
    /// # }
    /// ```ignore
    #[inline]
    pub async fn next_item(&mut self) -> Option<T> {
        StreamExt::next(self).await
    }
}

impl<'a, T: FromValue + Unpin + Send + 'a, C: AsyncConnection + Send + Unpin + 'a> Stream
    for AsyncIter<'a, T, C>
{
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let this = self.get_mut();
        let inner = std::mem::replace(&mut this.inner, IterOrFuture::Empty);
        match inner {
            IterOrFuture::Iter(mut iter) => {
                let fut = async move {
                    let next_item = iter.next_item().await;
                    (iter, next_item)
                };
                this.inner = IterOrFuture::Future(Box::pin(fut));
                Pin::new(this).poll_next(cx)
            }
            IterOrFuture::Future(mut fut) => match fut.as_mut().poll(cx) {
                Poll::Pending => {
                    this.inner = IterOrFuture::Future(fut);
                    Poll::Pending
                }
                Poll::Ready((iter, value)) => {
                    this.inner = IterOrFuture::Iter(iter);
                    Poll::Ready(value)
                }
            },
            IterOrFuture::Empty => Poll::Ready(None),
        }
    }
}

fn countdigits(mut v: usize) -> usize {
    let mut result = 1;
    loop {
        if v < 10 {
            return result;
        }
        if v < 100 {
            return result + 1;
        }
        if v < 1000 {
            return result + 2;
        }
        if v < 10000 {
            return result + 3;
        }

        v /= 10000;
        result += 4;
    }
}

#[inline]
fn bulklen(len: usize) -> usize {
    1 + countdigits(len) + 2 + len + 2
}

fn args_len<'a, I>(args: I, cursor: u64) -> usize
where
    I: IntoIterator<Item = Arg<&'a [u8]>> + ExactSizeIterator,
{
    let mut total_len = countdigits(args.len()).saturating_add(3);
    for item in args {
        total_len += bulklen(match item {
            Arg::Cursor => {
                // Use itoa to get exact digit count, matching what write_command uses.
                let mut buf = itoa::Buffer::new();
                buf.format(cursor).len()
            }
            Arg::Simple(val) => val.len(),
        });
    }
    total_len
}

pub(crate) fn cmd_len(cmd: &impl Borrow<Cmd>) -> usize {
    let cmd_ref: &Cmd = cmd.borrow();
    args_len(cmd_ref.args_iter(), cmd_ref.cursor.unwrap_or(0))
}

fn write_command_to_vec<'a, I>(cmd: &mut Vec<u8>, args: I, cursor: u64, is_fenced: bool)
where
    I: IntoIterator<Item = Arg<&'a [u8]>> + Clone + ExactSizeIterator,
{
    let total_len =
        args_len(args.clone(), cursor) + if is_fenced { FENCE_COMMAND.len() } else { 0 };

    cmd.reserve(total_len);

    write_command(cmd, args, cursor, is_fenced).unwrap()
}

fn write_command<'a, I>(
    cmd: &mut (impl ?Sized + io::Write),
    args: I,
    cursor: u64,
    is_fenced: bool,
) -> io::Result<()>
where
    I: IntoIterator<Item = Arg<&'a [u8]>> + Clone + ExactSizeIterator,
{
    let mut buf = ::itoa::Buffer::new();

    cmd.write_all(b"*")?;
    let s = buf.format(args.len());
    cmd.write_all(s.as_bytes())?;
    cmd.write_all(b"\r\n")?;

    let mut cursor_bytes = itoa::Buffer::new();
    for item in args {
        let bytes = match item {
            Arg::Cursor => cursor_bytes.format(cursor).as_bytes(),
            Arg::Simple(val) => val,
        };

        cmd.write_all(b"$")?;
        let s = buf.format(bytes.len());
        cmd.write_all(s.as_bytes())?;
        cmd.write_all(b"\r\n")?;

        cmd.write_all(bytes)?;
        cmd.write_all(b"\r\n")?;
    }

    // If this is a fenced command, append a PING command
    if is_fenced {
        cmd.write_all(FENCE_COMMAND)?;
    }

    Ok(())
}

impl Write for Cmd {
    fn write_arg(&mut self, arg: &[u8]) {
        self.data.extend_from_slice(arg);
        self.args.push(Arg::Simple(self.data.len()));
    }

    fn write_arg_fmt(&mut self, arg: impl fmt::Display) {
        use std::io::Write;
        write!(self.data, "{arg}").unwrap();
        self.args.push(Arg::Simple(self.data.len()));
    }
}

impl Default for Cmd {
    fn default() -> Cmd {
        Cmd::new()
    }
}

/// A command acts as a builder interface to creating encoded redis
/// requests.  This allows you to easily assemble a packed command
/// by chaining arguments together.
///
/// Basic example:
///
/// ```rust,ignore
/// ferriskey::Cmd::new().arg("SET").arg("my_key").arg(42);
/// ```ignore
///
/// There is also a helper function called `cmd` which makes it a
/// tiny bit shorter:
///
/// ```rust,ignore
/// ferriskey::cmd("SET").arg("my_key").arg(42);
/// ```ignore
///
/// Because Rust currently does not have an ideal system
/// for lifetimes of temporaries, sometimes you need to hold on to
/// the initially generated command:
///
/// ```rust,no_run,ignore
/// # let client = ferriskey::Client::open("redis://127.0.0.1/").unwrap();
/// # let mut con = client.get_connection(None).unwrap();
/// let mut cmd = ferriskey::cmd("SMEMBERS");
/// let mut iter : ferriskey::Iter<i32> = cmd.arg("my_set").clone().iter(&mut con).unwrap();
/// ```ignore
impl Cmd {
    /// Creates a new empty command.
    pub fn new() -> Cmd {
        Cmd {
            data: vec![],
            args: vec![],
            cursor: None,
            span: None,
            is_fenced: false,
            inflight_tracker: None,
        }
    }

    /// Get the capacities for the internal buffers.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn capacity(&self) -> (usize, usize) {
        (self.args.capacity(), self.data.capacity())
    }

    /// Appends an argument to the command.  The argument passed must
    /// be a type that implements `ToArgs`.  Most primitive types as
    /// well as vectors of primitive types implement it.
    ///
    /// For instance all of the following are valid:
    ///
    /// ```rust,ignore
    /// # let client = ferriskey::Client::open("redis://127.0.0.1/").unwrap();
    /// # let mut con = client.get_connection(None).unwrap();
    /// ferriskey::cmd("SET").arg(&["my_key", "my_value"]);
    /// ferriskey::cmd("SET").arg("my_key").arg(42);
    /// ferriskey::cmd("SET").arg("my_key").arg(b"my_value");
    /// ```ignore
    #[inline]
    pub fn arg<T: ToArgs>(&mut self, arg: T) -> &mut Cmd {
        arg.write_args(self);
        self
    }


    /// Returns the packed command as Bytes.
    #[inline]
    pub fn get_packed_command(&self) -> bytes::Bytes {
        let mut cmd = Vec::new();
        self.write_packed_command(&mut cmd);
        cmd.into()
    }

    pub(crate) fn write_packed_command(&self, cmd: &mut Vec<u8>) {
        write_command_to_vec(
            cmd,
            self.args_iter(),
            self.cursor.unwrap_or(0),
            self.is_fenced,
        )
    }

    pub(crate) fn write_packed_command_preallocated(&self, cmd: &mut Vec<u8>) {
        write_command(
            cmd,
            self.args_iter(),
            self.cursor.unwrap_or(0),
            self.is_fenced,
        )
        .unwrap()
    }

    /// Sends the command as query to the async connection and converts the
    /// result to the target valkey value.
    #[inline]
    pub async fn query_async<C, T: FromValue>(&self, con: &mut C) -> Result<T>
    where
        C: crate::connection::ConnectionLike,
    {
        let val = con.req_packed_command(self).await?;
        from_owned_value(val)
    }

    /// Returns an iterator over the arguments in this command (including the command name itself)
    pub(crate) fn args_iter(&self) -> impl Clone + ExactSizeIterator<Item = Arg<&[u8]>> {
        let mut prev = 0;
        self.args.iter().map(move |arg| match *arg {
            Arg::Simple(i) => {
                let arg = Arg::Simple(&self.data[prev..i]);
                prev = i;
                arg
            }

            Arg::Cursor => Arg::Cursor,
        })
    }

    // Get a reference to the argument at `idx`.
    // Returns `None` if `idx` is out of bounds or points to a Cursor argument.
    pub(crate) fn arg_idx(&self, idx: usize) -> Option<&[u8]> {
        if idx >= self.args.len() {
            return None;
        }

        // The target argument must be Simple; Cursor args have no data.
        let end = match self.args[idx] {
            Arg::Simple(n) => n,
            Arg::Cursor => return None,
        };

        // Walk backwards to find the nearest preceding Simple arg's end offset,
        // which is where this argument's data starts. Cursor args don't advance
        // the data offset, so we must skip over them.
        let start = if idx == 0 {
            0
        } else {
            let mut i = idx - 1;
            loop {
                match self.args[i] {
                    Arg::Simple(n) => break n,
                    Arg::Cursor => {
                        if i == 0 {
                            break 0;
                        }
                        i -= 1;
                    }
                }
            }
        };

        Some(&self.data[start..end])
    }

    /// Return this command span
    #[inline]
    pub(crate) fn span(&self) -> Option<FerrisKeySpan> {
        self.span.clone()
    }

    /// Mark this command as fenced. A PING command will be appended after it
    /// to ensure proper ordering of response processing.
    #[inline]
    pub(crate) fn set_fenced(&mut self, fenced: bool) -> &mut Cmd {
        self.is_fenced = fenced;
        self
    }

    /// Check whether this command is fenced.
    #[inline]
    pub(crate) fn is_fenced(&self) -> bool {
        self.is_fenced
    }

    /// Attach an inflight slot tracker. The slot is released when the last
    /// clone of this Cmd (or its `Arc<Cmd>`) is dropped.
    #[inline]
    pub(crate) fn set_inflight_tracker(&mut self, tracker: crate::value::InflightRequestTracker) {
        self.inflight_tracker = Some(tracker);
    }
}

impl fmt::Debug for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = self
            .args_iter()
            .map(|arg| {
                let bytes = match arg {
                    Arg::Cursor => b"<scan_cursor>",
                    Arg::Simple(val) => val,
                };
                std::str::from_utf8(bytes).unwrap_or_default()
            })
            .collect::<Vec<_>>();
        f.debug_struct("Cmd").field("args", &res).finish()
    }
}

/// Shortcut function to creating a command with a single argument.
///
/// The first argument of a valkey command is always the name of the command
/// which needs to be a string.  This is the recommended way to start a
/// command pipe.
///
/// ```rust,ignore
/// ferriskey::cmd("PING");
/// ```ignore
pub fn cmd(name: &str) -> Cmd {
    let mut rv = Cmd::new();
    rv.arg(name);
    rv
}

/// Shortcut for creating a new pipeline.
pub fn pipe() -> Pipeline {
    Pipeline::new()
}

#[cfg(test)]
mod tests {
    use super::Cmd;

    #[test]
    fn test_cmd_arg_idx() {
        let mut c = Cmd::new();
        assert_eq!(c.arg_idx(0), None);

        c.arg("SET");
        assert_eq!(c.arg_idx(0), Some(&b"SET"[..]));
        assert_eq!(c.arg_idx(1), None);

        c.arg("foo").arg("42");
        assert_eq!(c.arg_idx(1), Some(&b"foo"[..]));
        assert_eq!(c.arg_idx(2), Some(&b"42"[..]));
        assert_eq!(c.arg_idx(3), None);
        assert_eq!(c.arg_idx(4), None);
    }
}
