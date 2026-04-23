# RFC-013 Round-2 — L (ergonomics / consumer)

**Bottom line:** ACCEPT

Re-reading as cairn consumer after revisions.

## Round-1 items verification

- **L1** `SuspendArgs::new` + `with_*`: §2.2.1 has the full constructor signature block. I can build a `SuspendArgs` without reaching inside a `#[non_exhaustive]` struct. GREEN.
- **L2** `WaitpointBinding::fresh()`: §2.2.1 shows it + `use_pending(&PendingWaitpoint)`. GREEN.
- **L3** `ResumePolicy::normal()`: §2.2.1 shows the body with explicit defaults. I don't have to guess what "normal" means. GREEN.
- **L4** Clock-skew: `timeout_at` doc explicitly calls out 10 min tolerance + points at `ff_sdk::diagnostics::server_time()`. GREEN.
- **L5** `waitpoint_key` mismatch: §2.4 + §4 both have it. GREEN.
- **L6** SDK auto-reconcile?: §5.1 explicitly says SDK does NOT auto-reconcile; caller owns the loop or uses `idempotency_key`. GREEN.
- **L7** `StateKind` is `#[non_exhaustive]`: §4 has the confirmation with file-line pin. GREEN.
- **L8** `SuspendOutcomeDetails`: §5.1 defines it explicitly. GREEN.
- **L9** `stop_renewal` ordering: §5.1 shows it inside the `Suspended` match arm with comment explaining the rationale. No more lease bleed on transport error. GREEN.

## Per-section re-scan from consumer POV

- §2.2 GREEN. My concern about "how do I get a valid `now`?" — `SystemTime::now()` defaults inside `build_suspend_args` per §5.1; I don't actually have to construct `SuspendArgs` by hand unless I'm calling the trait directly.
- §2.2.1 GREEN — `SuspendArgs::new(suspension_id, waitpoint, resume_condition, resume_policy, reason_code, now)` is 6 required params. That's livable. The with_* chain covers the rest.
- §2.3 GREEN.
- §2.4 GREEN.
- §2.5 GREEN — `ResumePolicy::normal()` is what I want 95% of the time.
- §2.6 GREEN — enums are ergonomic to match.
- §3 GREEN — retry contract is clear.
- §4 GREEN — new rows + `StateKind` confirmation make the match ladder tractable.
- §5.1 GREEN — split is clean, `stop_renewal` reorder is correct. `SuspendOutcomeDetails` is defined.
- §5.2 GREEN — migration note is actionable.
- §6–§9 GREEN.

One micro-nit I'll NOT raise as a dissent driver: `SuspendArgs::new` takes `now: TimestampMs` as a required param even though the SDK always injects `SystemTime::now()`. For direct-trait consumers this is friction. But it's a deliberate choice (don't hide clock sources in constructors) and the SDK wrapper hides it. Not a blocker.

No remaining dissent.
