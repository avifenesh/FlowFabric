//! Handler-visible context: a borrowed [`crate::ClaimedTask`] + the
//! runtime's state map. Each [`crate::runtime::FromTask`] impl
//! resolves against this before the handler body runs.

use crate::ClaimedTask;

use super::registry::StateMap;

/// Read-only context passed to extractors during dispatch.
///
/// Borrowed — does not own the task. The task stays owned by the
/// runtime dispatcher until the handler's result is mapped into
/// `complete` / `fail`.
pub struct RuntimeCtx<'t> {
    task: &'t ClaimedTask,
    state: &'t StateMap,
}

impl<'t> RuntimeCtx<'t> {
    pub(crate) fn new(task: &'t ClaimedTask, state: &'t StateMap) -> Self {
        Self { task, state }
    }

    /// The claimed task. Extractors that need the whole
    /// [`ClaimedTask`] (e.g. [`super::Task`]) reach it through here.
    pub fn task(&self) -> &'t ClaimedTask {
        self.task
    }

    /// The registry of typed state values handed in via
    /// [`super::WorkerRuntime::data`].
    pub fn state(&self) -> &'t StateMap {
        self.state
    }
}
