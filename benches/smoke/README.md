# ff-smoke-v0.7 — pre-release smoke harness

Lives inside the isolated `benches/` workspace so smoke-only
dependencies (`reqwest`, `sqlx`) never leak into product crates.

## Purpose

Gate release `v0.7.x`. The v0.6.0 release shipped with a broken
`read_summary` because the smoke ran **after** `cargo publish`
(see `feedback_smoke_before_release.md`). This crate flips that
sequence: smoke runs **before** tagging, against workspace-path
dependencies that match the shape of the about-to-be-published
artifacts bit-for-bit.

## Layout

```
benches/smoke/
├── Cargo.toml
├── README.md
└── src/
    ├── main.rs                          # CLI + report printer
    └── scenarios/
        ├── mod.rs                       # shared helpers + dispatch
        ├── claim_lifecycle.rs           # create → claim → progress → complete
        ├── flow_anyof.rs                # AnyOf{CancelRemaining} DAG reachability
        ├── suspend_signal.rs            # suspend + deliver_signal (RFC-013/014)
        ├── stream_durable_summary.rs    # *** read_summary — the v0.6.0 regression ***
        ├── stream_best_effort.rs        # read_stream / tail_stream probe
        ├── cancel_cascade.rs            # cancel routing through dispatcher
        └── fanout_slo.rs                # 50-way ingress + claim-pump observation
```

## Backends

Two backends, one scenario loop:

| Leg       | Path                                                          | Rationale                                                              |
| --------- | ------------------------------------------------------------- | ---------------------------------------------------------------------- |
| Valkey    | HTTP against `ff-server` on `$FF_SMOKE_SERVER`                | Matches v0.6.1 consumer posture — proves nothing regressed there.      |
| Postgres  | Direct `ff_core::EngineBackend` trait + inherent `create_*`   | No HTTP frontend for Postgres in wave 7; trait is the only entry.      |

## Scenario status contract

Each scenario returns one of:

* **Pass** — the surface responded correctly.
* **Skip** — the surface is not yet reachable on this backend (e.g.
  `EngineError::Unavailable`, or a waves-pending gap). Captured with
  a human-readable reason.
* **Fail** — the surface is reachable but responded incorrectly, 5xx'd,
  or faulted with a transport error.

`--strict` promotes Skip to Fail; the release gate must run with
`--strict`.

## Cross-backend parity

When `--backend both`, the main loop compares status per scenario
across the two backends. Any asymmetry (Pass on one, Skip/Fail on
the other) fires a parity violation and fails the run regardless
of `--strict`. This guards against "Valkey is green, Postgres is
Skip, release anyway."

## Invoking directly

```bash
# With Valkey + Postgres fixtures already up:
cargo run -p ff-smoke-v07 --release --manifest-path benches/Cargo.toml -- \
    --backend both --strict
```

Normal operator path goes through `scripts/smoke-v0.7.sh`, which
handles fixture bring-up + `ff-server` launch.

## Adding a scenario

1. Drop a new `src/scenarios/<name>.rs` following the two-fn
   template (`run_valkey`, `run_postgres`).
2. Add it to the `NAMES` const in `scenarios/mod.rs`.
3. Wire it into `run_all_valkey` + `run_all_postgres`.
4. Verify the cross-backend parity rule makes sense for the new
   scenario — scenarios whose Valkey and Postgres paths have
   legitimately different status expectations should be excluded
   from the parity comparison (extend `NAMES` with per-scenario
   metadata if that case arises).

## What this smoke does NOT cover

* Perf / p99 SLO validation — lives in `benches/harness/benches/`
  (Wave 7c sibling).
* Full ff-test matrix — lives in `crates/ff-test` (Wave 7a sibling).
* Post-publish crates.io resolvability check — see §"After tag + publish"
  in `docs/RELEASING.md`.
