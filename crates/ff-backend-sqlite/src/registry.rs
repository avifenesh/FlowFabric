//! Process-local `SqliteBackend` registry (RFC-023 §4.2 B6).
//!
//! Enforces the "one `SqliteBackend` per file-path per process"
//! invariant: multiple `SqliteBackend::new(path)` calls to the same
//! canonicalized path return `Arc`-clones of the same underlying
//! handle so broadcast channels + in-proc pub-sub stay consistent.
//! Distinct `:memory:` URIs (which embed per-call UUIDs) get distinct
//! entries by construction.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::backend::SqliteBackendInner;

/// Global registry of live SQLite backends keyed by canonical path
/// (or the verbatim `:memory:` / `file::memory:...` URI when
/// canonicalization is skipped).
static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<SqliteBackendInner>>>> =
    OnceLock::new();

fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<SqliteBackendInner>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Look up an existing backend by key. Returns `Some(clone)` when a
/// live entry exists; `None` when absent or the weak handle has
/// decayed.
pub(crate) fn lookup(key: &PathBuf) -> Option<Arc<SqliteBackendInner>> {
    let guard = registry().lock().ok()?;
    guard.get(key).and_then(Weak::upgrade)
}

/// Install a new backend in the registry. Idempotent under the
/// key-present-live branch: if another thread installed between
/// `lookup` and `insert`, returns the existing entry instead of
/// overwriting.
pub(crate) fn insert(
    key: PathBuf,
    inner: Arc<SqliteBackendInner>,
) -> Arc<SqliteBackendInner> {
    let mut guard = match registry().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(existing) = guard.get(&key).and_then(Weak::upgrade) {
        return existing;
    }
    guard.insert(key, Arc::downgrade(&inner));
    inner
}
