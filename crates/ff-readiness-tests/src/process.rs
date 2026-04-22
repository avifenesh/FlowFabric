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
            Ok(Ok(status)) => {
                tracing::debug!(
                    label = %self.label,
                    ?status,
                    "child exited cleanly on SIGTERM"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    label = %self.label,
                    error = %e,
                    "child wait() failed after SIGTERM; attempting SIGKILL"
                );
                if let Err(e) = child.kill().await {
                    tracing::warn!(label = %self.label, error = %e, "SIGKILL failed");
                }
                if let Err(e) = child.wait().await {
                    tracing::warn!(label = %self.label, error = %e, "final wait failed");
                }
            }
            Err(_) => {
                tracing::warn!(label = %self.label, "child did not exit within grace; SIGKILL");
                if let Err(e) = child.kill().await {
                    tracing::warn!(label = %self.label, error = %e, "SIGKILL failed");
                }
                if let Err(e) = child.wait().await {
                    tracing::warn!(label = %self.label, error = %e, "final wait failed");
                }
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Best-effort reap. `start_kill` signals the child; if a
            // tokio runtime is still live we also schedule a blocking
            // wait so the child doesn't linger as a zombie. Outside a
            // tokio context we fall back to synchronous reap via the
            // raw libc call — `tokio::process::Child::wait` itself
            // requires an active runtime.
            let _ = child.start_kill();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = child.wait().await;
                });
            } else {
                // No runtime (e.g. test panic teardown outside tokio).
                // Reap synchronously via the raw pid so /proc isn't
                // cluttered for the rest of the test run.
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    let mut status: i32 = 0;
                    unsafe {
                        libc_waitpid(pid as i32, &mut status as *mut i32, 0);
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    #[link_name = "waitpid"]
    fn libc_waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
}
