//! Handler trait + tuple-macro impls for 1..=4 args.
//!
//! A handler is an `async fn` (or closure returning a future) whose
//! arguments all implement [`FromTask`]. The trait is implemented for
//! concrete arities via the `impl_handler!` macro below, up to 4
//! arguments — rarely do worker handlers need more (real services
//! cap at ~3: one payload, one shared state, optionally one info
//! slot), and the compile-time cost of 16 arities is pure speculation
//! overhead per issue #331 scope reductions.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;

use futures::FutureExt as _;

use super::ctx::RuntimeCtx;
use super::extract::FromTask;
use super::HandlerOutcome;
use super::RuntimeError;

/// Handler trait — implemented for `Fn(A0, A1, ...) -> Fut` where
/// `Fut: Future<Output = Result<Option<Vec<u8>>, anyhow::Error>>` and
/// each `An: FromTask`.
///
/// A handler is an `async fn` (or closure returning a future) whose
/// arguments all implement [`FromTask`] and whose output is
/// [`HandlerResult`].
///
/// Consumers never name this trait directly; they pass an `async fn`
/// to [`super::WorkerRuntime::on`] and the blanket impls below
/// resolve. The `Args` type parameter is the tuple of extractor
/// types — carried purely to disambiguate the blanket impls.
pub trait Handler<Args: 'static>: Clone + Send + Sync + 'static {
    fn call(self, ctx: RuntimeCtx<'_>) -> HandlerFuture;

    /// Box the handler for dynamic dispatch in the runtime registry.
    fn into_boxed(self) -> BoxedHandler
    where
        Self: Sized,
    {
        BoxedHandler::new(self)
    }
}

/// Return type of [`Handler::call`]. Owned future so the dispatcher
/// can `.await` it inside the spawned task without propagating the
/// ctx lifetime.
pub type HandlerFuture = Pin<Box<dyn Future<Output = HandlerOutcome> + Send + 'static>>;

/// Type-erased handler stored in the runtime registry.
pub struct BoxedHandler {
    inner: Arc<dyn DynHandler>,
}

impl BoxedHandler {
    fn new<Args, H>(handler: H) -> Self
    where
        H: Handler<Args> + 'static,
        Args: 'static,
    {
        Self {
            inner: Arc::new(DynHandlerImpl {
                inner: handler,
                _args: std::marker::PhantomData::<fn(Args) -> ()>,
            }),
        }
    }

    pub(crate) fn clone_handler(&self) -> BoxedHandler {
        BoxedHandler {
            inner: self.inner.clone(),
        }
    }

    pub(crate) fn call(&self, ctx: RuntimeCtx<'_>) -> HandlerFuture {
        self.inner.call(ctx)
    }
}

trait DynHandler: Send + Sync {
    fn call(&self, ctx: RuntimeCtx<'_>) -> HandlerFuture;
}

struct DynHandlerImpl<H, Args> {
    inner: H,
    _args: std::marker::PhantomData<fn(Args) -> ()>,
}

impl<H, Args> DynHandler for DynHandlerImpl<H, Args>
where
    H: Handler<Args>,
    Args: 'static,
{
    fn call(&self, ctx: RuntimeCtx<'_>) -> HandlerFuture {
        self.inner.clone().call(ctx)
    }
}

/// Return type consumer handlers produce.
///
/// - `Ok(None)` → task completes with no payload.
/// - `Ok(Some(bytes))` → task completes; bytes passed to
///   [`crate::ClaimedTask::complete`].
/// - `Err(e)` → task fails; `e` is stringified into the failure
///   reason and threaded to [`crate::ClaimedTask::fail`] with
///   `error_category = "handler_error"`.
///
/// The error type is a trait-object — accepts anything that
/// implements `std::error::Error + Send + Sync`, including
/// `anyhow::Error`, `thiserror`-derived enums, and `Box<dyn
/// std::error::Error>`-returning library functions. No dep on
/// `anyhow` is imposed.
pub type HandlerResult =
    Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync + 'static>>;

/// Wrap a handler future with panic-catch + Result → HandlerOutcome
/// mapping. Centralized so each arity's blanket impl stays one line.
fn wrap_future<Fut>(fut: Fut) -> HandlerFuture
where
    Fut: Future<Output = HandlerResult> + Send + 'static,
{
    Box::pin(async move {
        match AssertUnwindSafe(fut).catch_unwind().await {
            Ok(Ok(payload)) => HandlerOutcome::Complete(payload),
            Ok(Err(e)) => HandlerOutcome::handler_error(e),
            Err(panic_box) => {
                let msg = panic_message(panic_box);
                HandlerOutcome::panic(msg)
            }
        }
    })
}

fn panic_message(panic_box: Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = panic_box.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = panic_box.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

/// Arity-0 handler (no extractors).
impl<F, Fut> Handler<()> for F
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = HandlerResult> + Send + 'static,
{
    fn call(self, _ctx: RuntimeCtx<'_>) -> HandlerFuture {
        wrap_future(self())
    }
}

/// Macro expanding one arity's blanket impl. Extracts each argument
/// in order; the first extractor failure short-circuits to
/// [`HandlerOutcome::runtime_error`].
macro_rules! impl_handler_arity {
    ($($ty:ident),+) => {
        impl<F, Fut, $($ty,)+> Handler<($($ty,)+)> for F
        where
            F: Fn($($ty),+) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = HandlerResult> + Send + 'static,
            $($ty: FromTask,)+
        {
            #[allow(non_snake_case)]
            fn call(self, ctx: RuntimeCtx<'_>) -> HandlerFuture {
                $(
                    let $ty = match <$ty as FromTask>::from_task(&ctx) {
                        Ok(v) => v,
                        Err(e) => return extractor_err(e),
                    };
                )+
                wrap_future(self($($ty),+))
            }
        }
    };
}

/// Build a short-circuit future for an extractor failure.
fn extractor_err(err: RuntimeError) -> HandlerFuture {
    Box::pin(async move { HandlerOutcome::runtime_error(err) })
}

impl_handler_arity!(A0);
impl_handler_arity!(A0, A1);
impl_handler_arity!(A0, A1, A2);
impl_handler_arity!(A0, A1, A2, A3);
