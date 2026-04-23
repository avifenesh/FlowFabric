# apalis comparison (2026-04-23)

Investigator: Worker VV. Report-only; issue avifenesh/FlowFabric#51. Cites
apalis repo `apalis-dev/apalis` (fetched via `gh api` 2026-04-23). Note:
the GitHub handle in the issue (`geofmureithi`) is the maintainer; the
canonical repo org has since moved to `apalis-dev/apalis` per the README
badges ([README](https://github.com/apalis-dev/apalis)).

## Issue #51 summary

Owner avifenesh opened #51 asking for an apalis/FlowFabric comparison.
Maintainer geofmureithi replied with two questions and one substantive
technical claim (verbatim):

> 1. How does apalis compare with FlowFabric? What features do you
>    think are missing or what parts are performing poorly?
> 2. Are you open to a PR that adds some upgrades your approach with
>    best practices and shared connections? This would give a more
>    realistic comparison.
>
> Ps in scenario 4, apalis offers apalis-workflow which has sequential
> and dag primitives and would be better than the current approach (I
> hope).

Source: `gh issue view 51 --repo avifenesh/FlowFabric`.

The "better method" claim is narrow and specific: it is about
**scenario 4** in `benches/comparisons/apalis/` (the 10-stage linear
chain, currently built in FF's bench harness as manual stage chaining
with a dummy `flow_id` token — see
`/home/ubuntu/FlowFabric/benches/comparisons/apalis/COMPARISON.md`).
The maintainer is saying: don't hand-roll the apalis side of scenario
4 with plain `WorkerBuilder` + inter-queue pushes; use
**`apalis-workflow`**, which has first-class sequential and DAG
primitives. The owner accepted ("very open — if it helps surface what
I should do better") and offered to re-run after the PR lands.

There is no claim that apalis's architecture is globally "a better
method" than FF's — only that FF's current scenario-4 apalis harness
under-represents apalis by not using `apalis-workflow`.

## apalis architecture

Sources: apalis root [README.md](https://github.com/apalis-dev/apalis/blob/main/README.md),
`apalis-workflow/README.md`, `apalis-core/src/` directory listing.

- **Positioning**: "Type-safe, extensible, and high-performance
  background processing library for Rust." Topics: background-jobs,
  broker, job-scheduler, message-queue. Self-describes as a
  background-task library — not a flow engine.
- **Storage backends**: `apalis-redis`, `apalis-sqlite`,
  `apalis-postgres`, `apalis-mysql`, `apalis-amqp`, `apalis-cron`,
  `apalis-file-storage` (JsonStorage), in-memory. Each is its own
  crate. README crate-ecosystem table.
- **Core abstractions** (from `apalis-core/src/` tree: `backend/`,
  `monitor/`, `task/`, `task_fn/`, `worker/`): Task handlers are
  plain async functions (`async fn send_email(task: Email, data:
  Data<usize>)`); `WorkerBuilder::new(id).backend(storage).concurrency(n).build(fn).run()`.
  No macros. Dependency-injection via `Data<T>` extractors, explicitly
  inspired by axum/actix (README "Familiar dependency injection").
- **Middleware**: Tower-based — `.layer(...)` composes retries,
  timeouts, rate limits, tracing. README explicitly cites `tower` as
  "Extensible middleware system" and credits tower in the Thanks
  section.
- **Concurrency model**: per-worker polling of the backend; workers
  pull tasks, update status to `Running`/`Completed`. Sequence
  diagram in README. Runtime-agnostic (tokio, async-std).
- **Retries/backoff**: delivered as tower layers, not engine
  primitives. Heartbeat/ack is per-backend.
- **apalis-workflow** (the crate cited in #51,
  [README](https://github.com/apalis-dev/apalis/blob/main/apalis-workflow/README.md)):
  - Combinators: `and_then`, `filter_map`, `fold`, `delay_for` →
    sequential.
  - `DagFlow::new("id")` with `.node(fn)` + `.depends_on((&a, &b,
    &c))` → DAG; `.validate()?` checks acyclicity; Debug prints
    Graphviz dot.
  - Self-described properties: "Extensible, durable and resumable
    workflows. Workflows are processed in a distributed manner."
  - Backend support matrix: JsonStorage, SqliteStorage, RedisStorage,
    PostgresStorage, MysqlStorage all tick; workflows persist as
    state in the same storage the backend uses.
  - Inspirations listed: **Underway** (Postgres-stepped) and **dagx**
    (in-memory DAG). apalis-workflow is the durable DAG layer on
    whatever backend.

## FlowFabric divergence

Per-axis, grounded in FF RFCs in `/home/ubuntu/FlowFabric/rfcs/`:

| Axis | apalis | FlowFabric |
|------|--------|------------|
| Storage | Per-backend idiomatic crate; trait is backend-first | `EngineBackend` trait (RFC-012) abstracting over Valkey today, Postgres next (`project_ff_decoupling_plan.md`) |
| State transitions | Per-backend; JSON blob + status enum | FCALL-atomic Lua on Valkey with lease+fence triple (RFC-003) |
| Flow primitive | `apalis-workflow::DagFlow` — combinator + `.depends_on` tuple; persisted as state in backend | RFC-007 flow: engine-native DAG with step records, resumable via waitpoints |
| Reclaim on worker crash | Status heartbeat timeout; at-least-once re-delivery | Fenced lease + epoch — stale worker's writes rejected by Lua (RFC-003) |
| Suspension | None surfaced in README/workflow | RFC-004 waitpoints + HMAC tokens for external resume |
| Cross-cutting concerns | Tower layers (retry, timeout, rate-limit, tracing) at worker scope | Engine-level policies per RFC-005 (signal), RFC-008 (budget); observability via OTEL (owner lock 2026-04-22) |
| Handler ergonomics | `async fn(task, Data<T>) -> Result` — axum-flavor DI | `ff-sdk` with lower-op + higher-op traits; less axum-ish |
| Scheduling | `apalis-cron` + backend-supplied delay | RFC-009 scheduler (leader-elected, engine-native) |
| Concurrency contract | at-least-once (storage-dependent) | at-most-once under fenced lease (see COMPARISON.md "What's NOT comparable") |

## The "better method" — what they mean

Given #51's exact wording, the claim is **scoped to scenario 4**: FF's
current apalis harness chains `WorkerBuilder` instances across queues
with a dummy `flow_id` token (see `benches/comparisons/apalis/`,
scenario-4 bin). The maintainer is correctly pointing out that
`apalis-workflow::Workflow` (sequential combinators) or `DagFlow`
(dependency-declared nodes) is the idiomatic way to express the
10-stage chain in apalis 1.0-rc.7+. Using it would:

1. Remove the per-stage inter-queue push overhead (workflow stages
   share a single backend enqueue).
2. Persist workflow state once per step, not per inter-queue hop.
3. Make the apalis side of scenario-4 measure what apalis actually
   *ships* as a flow primitive, not a hand-rolled equivalent.

This is almost certainly a **fair bench-harness correction**, not a
claim about engine architecture. My reading (inference, not
maintainer's wording): numbers on scenario-4 will move in apalis's
favor once we adopt `apalis-workflow`, and the FF-vs-apalis flow
delta will narrow on the linear-chain shape. The DAG-shape and
failure-semantics comparisons in COMPARISON.md ("What's NOT
comparable") are unaffected.

## What's genuinely worth learning

Ordered by honest value to FF.

1. **Tower-layer composition for cross-cutting concerns.** apalis's
   retry/timeout/rate-limit story composes through `.layer(...)`.
   FF currently wires these as engine primitives (RFC-005,
   RFC-008) which is correct for atomicity but loses the
   user-space composability. *Worth a design look*: can FF expose a
   tower-compatible layer surface for purely-client-side concerns
   (local timeouts, local tracing spans, local rate limits) without
   compromising Lua-side atomicity? Non-blocking, no RFC required to
   investigate.

2. **Handler ergonomics via DI extractors.** `async fn(Task, Data<T>,
   WorkerContext)` is genuinely nicer than what ff-sdk currently
   surfaces for leaf handlers. Worth eyeballing when ff-sdk's
   higher-op trait lands (post-#135).

3. **Scenario-4 bench fix (the actual #51 ask).** Adopt
   `apalis-workflow` in `benches/comparisons/apalis/apalis-scenario4`.
   This is hygiene: our comparison numbers should represent apalis's
   best idiomatic shape, not a strawman. Note COMPARISON.md already
   labels the current harness `system = "apalis-approx"` explicitly
   to avoid overclaiming — that infrastructure is already honest;
   this just upgrades the numerator.

## What FF does that apalis can't / doesn't

Citations to FF RFCs.

- **Fenced atomicity**: RFC-003 lease with epoch fence — stale
  worker writes rejected at the Lua boundary. apalis's per-backend
  heartbeat has no equivalent fence on Redis (re-delivery races are
  possible on heartbeat expiry).
- **Waitpoints with HMAC resume tokens**: RFC-004. No apalis
  analogue visible in README or apalis-workflow README; workflow
  steps persist but don't surface an external-resume primitive.
- **Signal + budget as engine primitives**: RFC-005, RFC-008.
  apalis routes equivalents through tower layers, which means each
  backend's idiomatic impl reimplements the semantics.
- **Exec/flow co-location**: RFC-011 places step execution records
  adjacent to flow state in the same Valkey slot; apalis-workflow
  persists as generic backend rows with no such locality.
- **EngineBackend trait across heterogeneous stores**: RFC-012 +
  #58 decoupling plan. apalis's "multi-backend" is N independent
  crates with N idiomatic implementations; FF's direction is one
  trait, multiple impls — explicit Postgres-next path.

## Recommendation

**Close-the-issue shape**: not yet. Two concrete follow-ups.

1. **Accept the scenario-4 fix (bench-harness PR).** File a small
   issue (or accept geofmureithi's offered PR) updating
   `benches/comparisons/apalis/apalis-scenario4` to use
   `apalis-workflow::Workflow` for the 10-stage chain. Re-run
   scenario 4 post-merge. Update `benches/comparisons/apalis/COMPARISON.md`'s
   scenario-4 rows. Keep `system = "apalis-approx"` label only if
   something remains hand-rolled; otherwise relabel `system =
   "apalis"`.

2. **Spawn a non-blocking design note on tower-layer composition.**
   Separate from #51: a drafts/ note investigating whether FF can
   expose a tower-compatible user-space layer surface for
   client-local concerns without touching the Lua atomicity
   contract. Owner-directed, no commitment to implement.

**Do not**: adopt apalis-workflow as FF's flow model, or concede
that apalis's storage-first architecture is "a better method" in
the global sense. The maintainer didn't claim that, and FF's
divergence axes (fence, waitpoints, engine primitives) are load-bearing.

**Close-the-issue text** (proposed):

> Thanks for the pointer. You're right that scenario 4 under-represents
> apalis by hand-rolling the chain — we'll either accept your PR or
> land the `apalis-workflow` harness update ourselves and re-run.
> Scenarios 1–3 and the failure-semantics footnote in COMPARISON.md
> stand on their own merits. Parking the broader architectural
> comparison in a drafts note (`rfcs/drafts/apalis-comparison.md`).

## Citations

- Issue #51: `gh issue view 51 --repo avifenesh/FlowFabric`
- apalis README: https://github.com/apalis-dev/apalis/blob/main/README.md
- apalis-workflow README: https://github.com/apalis-dev/apalis/blob/main/apalis-workflow/README.md
- apalis repo metadata: `gh api repos/geofmureithi/apalis` (redirects to apalis-dev/apalis)
- FF COMPARISON.md: `/home/ubuntu/FlowFabric/benches/comparisons/apalis/COMPARISON.md`
- FF RFCs: `/home/ubuntu/FlowFabric/rfcs/RFC-003-lease.md`, `RFC-004-suspension.md`, `RFC-007-flow.md`, `RFC-011-exec-flow-colocation.md`, `RFC-012-engine-backend-trait.md`
