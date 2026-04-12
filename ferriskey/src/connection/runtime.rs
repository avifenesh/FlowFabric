use std::{io, time::Duration};

use crate::valkey::types::ValkeyError;
use futures_util::Future;

pub(crate) async fn timeout<F: Future>(
    duration: Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    ::tokio::time::timeout(duration, future)
        .await
        .map_err(|_| Elapsed(()))
}

#[derive(Debug)]
pub(crate) struct Elapsed(());

impl From<Elapsed> for ValkeyError {
    fn from(_: Elapsed) -> Self {
        io::Error::from(io::ErrorKind::TimedOut).into()
    }
}
