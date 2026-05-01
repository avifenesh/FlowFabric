# v0.13 cairn #454 — per-execution budget ledger

Headline demo for the four new `EngineBackend` trait methods introduced
in cairn #454. This example focuses on the clearest pair — the
per-execution budget ledger — and runs end-to-end against a local
Valkey instance.

## What it shows

The ledger shape (migration 0020 on PG/SQLite, per-execution HASH on
Valkey, Lua library v31) lets `release_budget` reverse a previously
recorded spend *exactly* without the caller needing to remember the
amount. This matters for cairn's rollback scenarios where a workflow
decides mid-flight that a recorded spend must not be billed.

- **`record_spend`** — stamps a per-execution row keyed by
  `(budget_id, execution_id, dimension)` and increments the aggregate
  counter.
- **`release_budget`** — reads the per-execution stamp, reverses the
  aggregate counter by the exact recorded amount, DELETEs the
  per-execution row. Missing-row is `Ok(())` — reversal is idempotent.

Expected output walks the budget usage hash through three states:

```
before:        []
after_spend:   [(cost_cents, 4200), (tokens, 250)]
after_release: [(cost_cents, 0),    (tokens, 0)]
```

Plus a replay with the same idempotency key returns `AlreadyApplied`
(no counter change) and a second `release_budget` on the same
`(budget_id, execution_id)` is a no-op.

The other two cairn #454 methods (`deliver_approval_signal`,
`issue_grant_and_claim`) require a full flow + waitpoint + grant state
setup; the existing `deploy-approval` and `incident-remediation`
examples already exercise the surrounding machinery. Parity and
integration test coverage for those lives in
`crates/ff-backend-{valkey,postgres,sqlite}/tests/typed_record_spend.rs`,
`typed_release_budget.rs`, `typed_deliver_approval_signal.rs`, and
`typed_issue_grant_and_claim.rs`.

## Run

```bash
cd examples/v013-cairn-454-budget-ledger
cargo run --bin budget-ledger
```

Connects to Valkey on `127.0.0.1:6379` by default. Override with
`FF_DEMO_VALKEY_HOST` / `FF_DEMO_VALKEY_PORT`.

## Backend coverage

All four cairn #454 methods ship bodies on Valkey + Postgres + SQLite
at v0.13.0 — see `docs/POSTGRES_PARITY_MATRIX.md §cairn #454
— control-plane additions (v0.13)`.
