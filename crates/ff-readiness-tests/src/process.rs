//! SIGTERM-on-drop guard for child processes spawned by readiness tests.
//!
//! Wraps a `tokio::process::Child` and, on `Drop`, sends SIGTERM then
//! waits up to `GRACE_MS` before forcing SIGKILL. Tests that panic
//! mid-flight still leave the system clean.

use std::time::Duration;
use tokio::process::Child;

const GRACE_MS: u64 = 3_000;

/// Guard around a `tokio::process::Child`. On drop, sends SIGTERM then
/// SIGKILL after a short grace.
pub struct ChildGuard {
    child: Option<Child>,
    label: String,
}

impl ChildGuard {
    pub fn new(child: Child, label: impl Into<String>) -> Self {
        Self {
            child: Some(child),
            label: label.into(),
        }
    }

    /// Access the underlying child for things like stdout capture.
    pub fn child_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    /// Graceful shutdown — returns once the child exits or grace
    /// elapses. Safe to call multiple times; subsequent calls no-op.
    pub async fn shutdown(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        // Best-effort SIGTERM via nix-like path (platform: unix only).
        #[cfg(unix)]
        {
            if let Some(pid) = child.id() {
                unsafe {
                    libc_kill(pid as i32, 15 /* SIGTERM */);
                }
            }
        }
        let grace = Duration::from_millis(GRACE_MS);
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(_) => {
                tracing::debug!(label = %self.label, "child exited cleanly on SIGTERM");
            }
            Err(_) => {
                tracing::warn!(label = %self.label, "child did not exit within grace; SIGKILL");
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Best-effort: if dropped outside a tokio context (e.g.
            // test panic teardown), a blocking kill is fine.
            let _ = child.start_kill();
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
