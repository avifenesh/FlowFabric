# FlowFabric — Use Cases and Primitive Design Workspace

## Why this document exists

This canvas keeps the full use-case surface in front of us while we define the core primitives. It is not the final public spec. It is the working reference that should constrain the design so we do not drift into a vague runtime or a dressed-up queue.

---

# Use-case inventory

## A. Baseline execution use cases

- **UC-01 — Fire-and-forget task**: Submit work asynchronously and let a worker pick it up later.
- **UC-02 — Delayed execution**: Run work no earlier than a given timestamp.
- **UC-03 — Scheduled/recurring execution**: Run work on cron or interval semantics.
- **UC-04 — Request/reply execution**: Submit work and wait synchronously or semi-synchronously for the result.
- **UC-05 — Priority execution**: Higher-priority work should preempt lower-priority waiting work.
- **UC-06 — Retry with backoff**: Retry failed execution with fixed, exponential, or custom backoff.
- **UC-07 — Dead-letter handling**: Move terminal failures into a durable failed state for inspection or replay.
- **UC-08 — Deduplicated submission**: Prevent duplicate or near-duplicate execution from being scheduled twice.
- **UC-09 — Batched execution**: Claim and process multiple compatible items together for throughput.
- **UC-10 — Queue-compatible execution mode**: Expose a familiar queue/worker model for teams migrating from BullMQ-style systems.

## B. Controlled lifecycle use cases

- **UC-11 — Lease-based ownership**: A worker claims execution and must renew ownership while it runs.
- **UC-12 — Crash recovery / stalled reclaim**: If a worker dies, the engine must detect abandonment and recover cleanly.
- **UC-13 — Cancellation / revocation**: An operator or upstream system can cancel work before or during execution.
- **UC-14 — Timeout enforcement**: Execution can expire based on wall-clock or lease rules.
- **UC-15 — Pause / drain / freeze intake**: Pause a queue or execution lane without losing in-flight visibility.
- **UC-16 — Manual requeue / replay**: An operator can re-enqueue or replay failed or stuck work.
- **UC-17 — State inspection**: At any point, the engine can explain whether work is waiting, active, delayed, blocked, failed, suspended, or completed.
- **UC-18 — Operator override**: A human can intervene: force-complete, force-fail, force-resume, reroute, or inject signals.

## C. Interruptible execution use cases

- **UC-19 — Suspend awaiting signal**: A running execution intentionally pauses and waits.
- **UC-20 — Resume from external callback**: A webhook, human, timer, or upstream system wakes suspended work.
- **UC-21 — Human approval gate**: Execution pauses for explicit approval or rejection.
- **UC-22 — Wait for multiple conditions**: Resume only after a set of signals, dependencies, or deadlines is satisfied.
- **UC-23 — Step continuation**: Execution pauses between steps and resumes later with continuation metadata.
- **UC-24 — Pause on policy or budget breach**: Execution suspends because a cost, token, or safety boundary was crossed.
- **UC-25 — External tool wait**: A job launches work elsewhere and waits for callback completion instead of blocking a worker.
- **UC-26 — Preemption / handoff**: Long execution can be safely handed to another worker or lane.

## D. Flow coordination use cases

- **UC-27 — Parent-child flow**: A parent execution launches child executions and waits for them.
- **UC-28 — DAG execution**: A graph of dependent executions runs according to dependencies, not just a tree.
- **UC-29 — Fan-out / fan-in**: Split work into many branches and join results later.
- **UC-30 — Dynamic child spawning**: New sub-executions are created during runtime, not fully known upfront.
- **UC-31 — Chain / stage pipeline**: A multi-step workflow runs in ordered stages with handoff between steps.
- **UC-32 — Waiting-children state**: An execution can remain alive while blocked on dependent work.
- **UC-33 — Partial aggregation**: Intermediate branch results can be collected before the whole flow finishes.
- **UC-34 — Flow-scoped failure policy**: One child failure may fail the flow, retry locally, or continue depending on policy.
- **UC-35 — Compensation / rollback choreography**: Later phase: flow can trigger compensating actions when a multi-step process breaks.

## E. AI-native execution use cases

- **UC-36 — Single LLM call with streamed output**: Run one inference and stream partial output as it arrives.
- **UC-37 — Token / cost accounting**: Track input tokens, output tokens, latency, and cost per execution.
- **UC-38 — Model/provider fallback**: On failure, retry, degrade, or reroute to alternate providers/models.
- **UC-39 — Flow-level budget enforcement**: A whole execution tree shares max token and/or max cost budgets.
- **UC-40 — Token-aware rate limiting**: Limit throughput by token volume, not just request count.
- **UC-41 — Long inference execution**: Support multi-minute or multi-stage model work without false stall detection.
- **UC-42 — Tool-calling agent step**: An execution can emit tool work, wait on results, then continue.
- **UC-43 — Planner/executor loop**: A higher-level execution can repeatedly plan, spawn work, observe outputs, and continue.
- **UC-44 — Concurrent sub-agent execution**: Multiple agent branches run in parallel under one coordinating flow.
- **UC-45 — Streaming artifacts, not just tokens**: Stream logs, frames, structured partial outputs, tool events, and intermediate results.
- **UC-46 — Evaluation / batch inference run**: Execute large prompt/eval batches with usage accounting and failure handling.
- **UC-47 — Human-in-the-loop generation**: Generation pauses for review, edit, or approval before continuing.
- **UC-48 — Provider quota / tenant quota enforcement**: Keep one tenant or provider from consuming the whole fleet.

## F. Routing and execution placement use cases

- **UC-49 — Capability-based routing**: Route execution to workers that advertise required capabilities.
- **UC-50 — Tenant-aware routing**: Keep execution isolated or affined by customer or workspace.
- **UC-51 — Model-tier routing**: Heavy reasoning work goes to one pool, cheap classification to another.
- **UC-52 — Isolation / sandbox routing**: Sensitive or risky work runs only in isolated workers.
- **UC-53 — Locality / region affinity**: Run work where data, network, or compliance constraints require it.
- **UC-54 — Fair scheduling across groups**: Prevent one group, tenant, or hot key from starving everything else.

## G. Control surface and operability use cases

- **UC-55 — Execution event feed**: Expose lifecycle events for observers and dashboards.
- **UC-56 — Live output feed**: Clients can subscribe to execution streams in real time.
- **UC-57 — Snapshot and diagnosis**: Inspect why an execution is blocked, waiting, rate-limited, suspended, or failing.
- **UC-58 — Worker health and fleet view**: Know which workers are alive, busy, stalled, or misbehaving.
- **UC-59 — Usage and budget summaries**: Aggregate costs and tokens across queues, groups, and flows.
- **UC-60 — Search and filtering**: Find executions by state, name, tenant, flow, time window, or tags.
- **UC-61 — Audit trail**: Retain enough history to explain what happened and why.
- **UC-62 — Language-neutral control API**: Submit, inspect, signal, and stream without forcing one SDK.

## H. Local/dev/test use cases

- **UC-63 — In-memory or local-dev mode**: Develop without provisioning a full distributed backend.
- **UC-64 — Deterministic testing mode**: Unit and integration tests can simulate timing, retries, suspend/resume, and signals.
- **UC-65 — Embedded single-node mode**: Small teams can run the engine locally before distributed deployment.

---

# Composed product scenarios

## AI and agent scenarios

- **SC-01 — Coding agent execution backend**
- **SC-02 — PR review / autofix system**
- **SC-03 — Research agent**
- **SC-04 — RAG document pipeline**
- **SC-05 — Multi-provider inference service**
- **SC-06 — Evaluation harness**
- **SC-07 — Human-reviewed generation pipeline**

## Workflow and automation scenarios

- **SC-08 — Business approval workflow**
- **SC-09 — External callback workflow**
- **SC-10 — Incident remediation workflow**
- **SC-11 — CI/CD gate and deploy workflow**
- **SC-12 — Data enrichment / ETL orchestration**
- **SC-13 — Media/doc processing pipeline**
- **SC-14 — Customer support automation**
- **SC-15 — Backoffice operations engine**

---

# What should not be core FlowFabric use cases

These may exist above or beside FlowFabric, but they should not define the execution core:

- long-term memory store
- vector database
- prompt registry
- eval product/UI
- approval product semantics
- business policy engine
- chat/session product model
- knowledge graph
- full operator dashboard as the main product
- generalized “support any backend equally” abstraction

---

# Identity conclusion

FlowFabric is not defined by enqueue + worker.

It is defined by:
- controlled execution
- interruption and resumption
- signals
- flow coordination
- resource-aware AI execution
- operator-grade visibility

That is why the category is execution engine / execution fabric, not queue.

---

# V1-defining use cases

These should define the first serious version:

1. Long-running execution with lease/recovery
2. Suspend / signal / resume
3. Flow execution: parent-child, fan-out/fan-in, DAG-lite
4. Streaming partial output
5. Usage and budget accounting
6. Fallback-aware AI execution
7. Capability-based routing
8. Operator inspection and control API
9. Queue-compatible submission mode

---

# Primitive design workspace

## Primitive 1 — Execution

### Purpose

Execution is the primary unit of controlled work in FlowFabric.

It is intentionally broader than a queue job and narrower than a product workflow.
An execution may be submitted through a queue-like interface, may belong to a flow, may suspend and later resume, may stream partial outputs, may consume shared budget, and may be routed to workers with specific capabilities.

Execution is the center of the model.
Queue, flow, routing, budgeting, signaling, and observability all attach to execution.

### Definition

An execution is a durable, inspectable, controllable unit of work with:
- identity
- lifecycle state
- input payload
- execution policy
- ownership / lease information when active
- optional parent / flow membership
- optional suspension state
- optional output stream
- usage and budget accounting
- retry / failure history
- control surface visibility

### Non-goals

Execution is not:
- the user-facing product workflow object
- the long-term business source of truth
- the prompt or memory object
- the queue itself
- the worker process itself

### Design requirements

Execution must support the following classes of behavior:
- asynchronous and synchronous submission
- delayed and scheduled eligibility
- active claiming by workers
- long-running ownership via lease renewal
- explicit suspension and signal-based resumption
- flow membership and dependency waiting
- retry and fallback progression
- partial output streaming
- token / cost accounting
- operator inspection and intervention

### Control versus signal boundary

For maintainability, active control should not be modeled as generic signals.

Examples of explicit control operations:
- cancel_execution
- revoke_lease
- reroute_execution
- pause_lane
- operator_override

Examples of resume/data signals:
- approval result
- callback payload
- tool result
- continue/resume input tied to a waitpoint

Rationale:
- easier invariants
- clearer audit semantics
- less ambiguity around active mutation rules
- simpler client expectations

### Core invariants

#### Invariant E1 — Stable identity
Each execution has a stable execution ID for its entire lifetime.
Retries, suspensions, resumes, streaming, and operator actions do not create a new logical execution unless explicitly requested by policy.

#### Invariant E2 — Explicit lifecycle state
At any moment, an execution has one authoritative lifecycle state.
State transitions must be explicit, auditable, and driven by engine operations.

#### Invariant E3 — Single active owner
An execution may have at most one active lease owner at a time.
Concurrent completion or conflicting mutations must be rejected or resolved atomically.

#### Invariant E4 — Durable control
Suspend, signal, resume, cancel, fail, retry, and complete are durable state transitions, not best-effort hints.

#### Invariant E5 — Inspectability
It must always be possible to inspect the execution’s current state, key timestamps, retry status, routing intent, and blocking reason.

#### Invariant E6 — Composability
Execution must be usable on its own, inside a queue facade, and inside a flow/DAG model without changing its identity or semantics.

#### Invariant E7 — Resource attachment
Usage, budgets, quotas, and fallbacks attach to execution and execution groups, not only to worker processes.

### Execution lifecycle states

These are conceptual states. Backend implementation may use multiple internal structures, but the external model must remain stable.

#### Submitted
Execution has been accepted by the engine but is not yet eligible for claiming.
Used for immediate submission before routing / scheduling resolution if needed.

#### Waiting
Execution is eligible to run and can be claimed by a worker.
This is the normal ready state.

#### Delayed
Execution is not yet eligible because of a future timestamp, backoff delay, or schedule policy.

#### Active
Execution is currently owned by a worker lease and is in progress.

#### Suspended
Execution intentionally paused and is waiting for signal, approval, condition, or timeout.
This is not failure and not abandonment.

#### WaitingChildren
Execution is blocked on dependent child executions or DAG prerequisites.

#### RateLimited
Execution is ready in principle but cannot proceed yet because of concurrency, quota, token, fairness, or policy constraints.

#### Completed
Execution finished successfully with final result metadata.

#### Failed
Execution finished unsuccessfully and is terminal unless explicitly retried or replayed.

#### Cancelled
Execution was intentionally terminated by user, policy, or operator action.

#### Expired
Execution became invalid because of deadline, TTL, or suspension timeout.

### State transition rules

#### Allowed high-level transitions
- Submitted -> Waiting
- Submitted -> Delayed
- Waiting -> Active
- Waiting -> Cancelled
- Waiting -> Delayed
- Waiting -> RateLimited
- Active -> Completed
- Active -> Failed
- Active -> Suspended
- Active -> WaitingChildren
- Active -> Delayed
- Active -> Cancelled
- Active -> Expired
- Suspended -> Waiting
- Suspended -> Cancelled
- Suspended -> Expired
- WaitingChildren -> Waiting
- WaitingChildren -> Failed
- WaitingChildren -> Cancelled
- RateLimited -> Waiting
- Delayed -> Waiting
- Failed -> Delayed (retry)
- Failed -> Waiting (manual replay or immediate retry)

#### Forbidden or tightly controlled transitions
- Completed -> Active
- Completed -> Waiting
- Cancelled -> Active
- Expired -> Active
- simultaneous Active ownership by multiple workers
- implicit resume without a recorded condition being satisfied

If replay semantics are needed, replay should either create a new execution or create an explicit replay transition with lineage preserved.

### Required execution fields

#### Identity
- execution_id
- namespace / queue / lane identifier
- submission_id or idempotency key if provided
- tenant / workspace / group scope if applicable

#### Payload
- execution kind / name
- input payload
- payload encoding metadata
- optional tags / labels

#### Policy
- priority
- delay / schedule metadata
- retry policy
- timeout / expiration policy
- suspension policy
- fallback policy
- budget attachment
- routing / capability requirements
- deduplication or idempotency policy

#### Runtime state
- lifecycle state
- current attempt number
- processed / started timestamp
- completion timestamp
- failure reason if any
- cancellation reason if any
- blocking reason if any
- last state transition timestamp

#### Ownership
- current lease ID if active
- current worker / consumer identity if active
- lease expiration timestamp
- last heartbeat timestamp

#### Relationships
- parent execution ID if any
- flow ID if any
- dependency references if any
- child summary or dependency counters if needed

#### Output and result
- final result metadata
- output stream handle if enabled
- progress snapshot
- partial artifact pointers if any

#### Accounting
- token usage summary
- cost usage summary
- latency summary
- retry count / stall count
- current fallback index or route index

#### Audit
- creation timestamp
- creator/source identity
- last mutation timestamp
- last operator action metadata

### Minimal operations on execution

#### Submission operations
- create_execution
- create_delayed_execution
- create_scheduled_execution
- create_or_get_deduplicated_execution

#### Claim / ownership operations
- claim_execution
- renew_execution_lease
- release_execution_lease
- recover_abandoned_execution

#### Runtime mutation operations
- complete_execution
- fail_execution
- cancel_execution
- expire_execution
- delay_execution
- move_execution_to_waiting_children
- suspend_execution
- resume_execution
- signal_execution

#### Retry / replay operations
- retry_execution
- replay_execution
- change_priority
- change_schedule_or_delay

#### Output / accounting operations
- append_execution_output
- update_execution_progress
- report_execution_usage
- record_execution_artifact

#### Inspection operations
- get_execution
- get_execution_state
- get_execution_history_summary
- get_execution_blocking_reason
- get_execution_stream_info
- get_execution_usage

### Blocking model

Every non-progressing non-terminal execution should expose one blocking reason category.

Initial categories:
- waiting_for_worker
- delayed_until_time
- waiting_for_signal
- waiting_for_approval
- waiting_for_children
- waiting_for_budget
- waiting_for_quota
- waiting_for_capable_worker
- waiting_for_retry_backoff
- paused_by_operator
- expired

This is important because “why is this stuck?” is one of the most common operator questions.

### Execution result model

Execution result should separate terminal outcome from streamed or intermediate output.

#### Final outcome
- success with final value / metadata
- failure with structured reason
- cancelled with structured reason
- expired with structured reason

#### Intermediate output
- progress updates
- stream frames
- logs
- structured artifacts
- usage updates
- signal receipts

### Execution lineage

FlowFabric should preserve lineage across retries, fallbacks, and optional replay.

At minimum:
- stable execution ID across retries, reclaims, and replays
- attempt index
- fallback index lineage
- parent / flow linkage
- replay metadata within the same execution history

### Execution visibility levels

FlowFabric should support visibility without forcing every caller to see every detail.

Suggested levels:
- public summary
- operator summary
- internal engine detail

This matters for multi-tenant and product-integrated scenarios.

### Open design questions

These are intentionally unresolved and should become RFC material later.

1. Should replay reuse execution identity or always create a new execution with lineage?
2. Should suspension be a top-level state only, or also a subtype of blocked?
3. Should request/reply be modeled as a submission mode or a separate facade over execution waiters?
4. How much dependency state should live directly on the execution record versus in separate flow indexes?
5. Should streamed output be guaranteed durable by default or configurable per execution class?

---

## Primitive 1.5 — Attempt / Run

### Purpose

Attempt is the concrete execution run record for one logical execution episode.

Execution is the stable logical unit of work.
Attempt is one concrete try to run that work.

This distinction is required for:
- retries
- fallback progression
- lease lineage
- per-attempt usage and latency
- operator debugging
- route and worker attribution
- replay history
- reclaim history

Without Attempt, execution history becomes muddy very quickly.

### Definition

An attempt is a durable or semi-durable sub-record of an execution representing one concrete runnable try.
A single execution may have many attempts over its lifetime.

Examples:
- initial try = attempt 1
- retry after failure = attempt 2
- reclaim after lease expiry = attempt 3
- replay after terminal completion/failure = attempt 4

### Core invariants

#### Invariant A1 — Execution identity is stable across attempts, retries, reclaims, and replays
Attempts do not replace execution identity.
They refine it.

#### Invariant A2 — Attempt ordering is monotonic
Each new attempt gets a strictly increasing attempt index within its execution, regardless of whether it came from retry, reclaim, or replay.

#### Invariant A3 — One active attempt per execution
At any moment, an execution may have at most one active attempt.

#### Invariant A4 — Lease belongs to an attempt
The current lease should attach to the current active attempt, not only to the execution generically.

#### Invariant A5 — Usage attribution should be attempt-aware
Latency, fallback selection, route choice, and usage should be attributable to the attempt that produced them.

### Attempt fields

#### Identity
- attempt_id
- execution_id
- attempt_index

#### Timing
- created_at
- started_at
- ended_at

#### Outcome
- attempt_outcome
- failure_reason if any
- terminalized_by_override flag if any
- interrupted_by_reclaim flag if any
- replay_reason if any

#### Ownership / placement
- current_or_last_lease_id
- worker_id / worker_instance_id
- route_snapshot
- capability snapshot if useful

#### AI-specific context
- fallback_index
- provider/model selection snapshot
- usage summary
- latency summary

#### Lineage
- retry_reason
- replay_requested_by if any
- replayed_from_attempt_index if any

### Attempt lifecycle

- created
- started
- ended_success
- ended_failure
- ended_cancelled
- interrupted_reclaimed
- superseded_by_retry
- superseded_by_replay

This does not need to be a heavyweight public state machine, but the engine should persist enough data to explain what happened.

### When to create a new attempt

A new attempt should be created for:
- retry after failure
- retry after retryable timeout when that is treated as a new semantic try
- fallback progression that semantically restarts model execution
- reclaim after lease expiry
- explicit replay of the execution

A new attempt should not be created for:
- lease renewal
- operator inspection
- stream tailing or signal delivery
- normal signal receipt while suspended

### Public API stance

Attempt should be a first-class inspectable object in the public model, but not the primary entry point.

That means:
- normal UX remains execution-centric
- deep inspection, debugging, and operator APIs may fetch attempt history directly
- execution inspection should include attempt summaries

### Open design questions

1. How much of attempt history is required in v1 list APIs versus deep inspection APIs?
2. Should attempt summaries always include route and worker attribution, or can some of that remain deep-inspection only?

## Primitive 2 — Lease

### Purpose

Lease is the bounded ownership contract that allows one worker to control one active execution for a limited period.

Lease exists to solve:
- single active ownership
- duplicate-completion protection
- long-running execution with periodic liveness renewal
- safe reclaim after worker crash or disconnection
- cooperative handoff when execution should move elsewhere

Lease is not merely a lock.
It is the engine’s proof of who is allowed to mutate active execution state.

### Definition

A lease is a time-bounded, versioned ownership record attached to an active execution.
A valid lease grants a specific worker the right to perform active-state mutations for that execution until the lease expires, is released, is transferred, or is revoked.

### Core invariants

#### Invariant L1 — At most one valid lease
At any time, an execution may have at most one valid lease.
Multiple historical lease records may exist in audit history, but only one may be current and authoritative.

#### Invariant L2 — Lease required for active mutation
Normal active-state mutations require a matching valid lease.
This includes:
- complete_execution
- fail_execution
- suspend_execution
- delay_execution from active state
- move_execution_to_waiting_children
- progress and usage mutation when policy requires owner validation

#### Invariant L3 — Stale owner rejection
A worker that lost ownership must not be able to complete or otherwise mutate the execution as if it were still active.

This implies the engine needs a fencing concept, not only a TTL.

#### Invariant L4 — Expiration is observable
Lease expiry must be explicit and recoverable.
If a lease expires, the execution must transition into a reclaimable condition that operators and workers can inspect.

#### Invariant L5 — Suspension releases ownership
A suspended execution is not actively owned.
Entering Suspended must release or invalidate the active lease.

#### Invariant L6 — Completion clears ownership
Completed, Failed, Cancelled, and Expired executions must not retain a valid active lease.

### Required lease fields

#### Identity and fencing
- lease_id
- lease_epoch or fencing_token
- execution_id

#### Owner identity
- worker_id
- worker_instance_id or consumer identity
- optional worker capability snapshot
- optional lane / queue / route identifier

#### Timing
- acquired_at
- expires_at
- last_renewed_at
- recommended renewal_deadline or heartbeat_hint

#### Recovery / audit
- reclaim_count
- previous_lease_id if handed off or reclaimed
- revoked_at if revoked
- revoke_reason if any

### Fencing model

FlowFabric should use a monotonic fencing token or lease epoch per execution.
Each successful lease acquisition increments the epoch.
Any active-state mutation must include the lease identity or epoch and must be rejected if it is stale.

Why this matters:
- worker A may time out
- worker B may reclaim the execution
- worker A may still try to complete later

Without fencing, stale completion can corrupt correctness.

### Lease lifecycle

#### Acquire
A worker claims eligible execution and receives a new lease.
Acquisition transitions the execution into Active.

#### Renew
The current owner extends the lease before expiry.
Renewal updates liveness and keeps ownership stable.

#### Release
The current owner voluntarily releases the lease because execution completed, failed, suspended, moved to another state, or yielded cooperatively.

#### Expire
The lease times out because the owner stopped renewing.
The execution becomes reclaimable.

#### Reclaim
Another worker or a scheduler reclaims an execution whose previous lease expired.
Reclaim creates:
- a new lease with a higher fencing token
- a new attempt for the same logical execution

The interrupted prior attempt remains in history as interrupted/reclaimed, and the newly reclaimed work proceeds as the next attempt.

#### Revoke
The system or operator invalidates a lease intentionally.
Used for cancellation, worker drain, force handoff, or safety intervention.

#### Handoff
A cooperative owner yields control so another worker can take over with explicit lineage.
This is rarer than reclaim and should be treated as an advanced operation.

### Lease acquisition rules

A lease may only be acquired when:
- the execution is eligible to run
- the execution is not already validly leased
- routing / capability constraints are satisfied
- fairness, quota, and concurrency policy allow entry

Depending on state model, eligible sources include:
- Waiting
- Delayed once promoted
- RateLimited once released
- Suspended once resume conditions are satisfied and execution is re-queued
- WaitingChildren once dependencies resolve and execution becomes runnable again

### Renewal rules

Renewal must require:
- matching current lease identity
- matching current lease epoch
- execution still in a lease-bearing state
- not already terminal

Renewal should fail if:
- lease expired and was already reclaimed
- execution became terminal
- operator revoked ownership
- execution was suspended and ownership was dropped

### Expiration and reclaim rules

When a lease expires:
- the execution must not silently remain trusted as active forever
- the execution becomes reclaimable
- stale owner mutations must fail
- reclaim must be atomic and fence stale owners out

Open implementation choice:
- explicit intermediate state such as Recovering/Reclaimable
- or keep conceptual state Active with a reclaimable ownership condition

For the public model, the key requirement is inspectability, not the exact internal enum.

### Ownership conflict rules

If two workers contend for the same execution:
- only one may successfully acquire the new lease
- the loser must see a deterministic rejection
- no split-brain completion is allowed

If a stale owner tries to mutate after a successful reclaim:
- mutation must be rejected as stale lease
- the engine may emit an audit event for stale-owner attempt

### Interaction with execution transitions

#### Active -> Completed
Requires matching valid lease unless a privileged operator override is used.
Completion clears the lease.

#### Active -> Failed
Requires matching valid lease unless privileged override.
Failure clears the lease.

#### Active -> Suspended
Requires matching valid lease.
Suspension records state, drops active ownership, and leaves no valid lease behind.

#### Active -> WaitingChildren
Requires matching valid lease.
Execution becomes blocked on dependencies and should not retain active ownership.

#### Active -> Delayed
Requires matching valid lease.
Execution yields eligibility and releases ownership.

#### Active -> Cancelled
May happen from owner action or operator override.
Terminal cancellation must clear the lease.

### Operator interactions

The control surface should support:
- inspect current lease
- inspect current owner
- inspect remaining lease time
- inspect reclaim count
- revoke current lease
- drain worker and revoke its leases safely
- force reclaim after safety checks

### Minimal lease operations

- acquire_lease(execution_id, claimant)
- renew_lease(execution_id, lease_id, epoch)
- release_lease(execution_id, lease_id, epoch)
- revoke_lease(execution_id, lease_id, reason)
- reclaim_execution(execution_id)
- get_lease(execution_id)
- get_lease_history(execution_id)

### Error model

Useful explicit errors:
- lease_not_found
- lease_expired
- stale_lease
- lease_conflict
- lease_revoked
- execution_not_leaseable
- execution_not_active

### Open design questions

1. Should reclaim introduce a visible external state or remain an ownership condition only?
2. Do we need cooperative handoff in v1, or only expiration + reclaim?
3. Should progress and streaming require a valid lease on every append, or can they be slightly looser for throughput?
4. Should lease duration be per execution class, per queue/lane, or both?


## Primitive 3 — Suspension

### Purpose

Suspension is the durable blocked state used when execution intentionally pauses and waits.

This is one of the most important primitives in FlowFabric because it distinguishes controlled waiting from failure, abandonment, or retry.

Suspension exists to support:
- human approval
- webhook / callback wait
- external tool completion wait
- pause-for-policy
- wait for multiple signals
- step continuation
- long-lived interactive execution

### Definition

Suspension is an execution state entered intentionally from Active when the current owner decides that forward progress must pause until one or more resume conditions are met.

A suspended execution:
- is not running
- does not hold an active lease
- remains durable and inspectable
- may accumulate signals while suspended
- can later resume or expire

### Core invariants

#### Invariant S1 — Intentional entry
Suspension must be entered explicitly, not inferred from timeout or crash.
A crashed execution is reclaimable, not suspended.

#### Invariant S2 — No active ownership while suspended
A suspended execution must not retain a valid active lease.

#### Invariant S3 — Durable wait condition
The reason and conditions for suspension must be persisted and inspectable.

#### Invariant S4 — Resume is explicit
A suspended execution resumes only when resume conditions are satisfied by signal, operator action, deadline policy, or engine rule.
It must not spontaneously become active again without a recorded cause.

#### Invariant S5 — Signals survive the wait
Signals delivered while suspended must be durably recorded according to delivery policy.

### Suspension record

A suspension record should include:
- suspension_id
- execution_id
- waitpoint_id
- waitpoint_key if externally addressable
- suspension_reason_code
- optional human-readable reason
- requested_by (worker, operator, policy)
- created_at
- timeout_at if any
- resume_condition
- resume_policy
- continuation metadata pointer if any
- buffered signal summary
- last_signal_at

### Pending waitpoints

To avoid callback/approval races, the engine may support a **pending waitpoint** before full suspension commit.

That means:
- a waitpoint may be precreated and externally addressable before suspension is fully entered
- early signals may be accepted against that waitpoint
- when suspension commits, the waitpoint becomes attached to that suspension record
- if the execution never actually enters that waitpoint, the pending waitpoint must expire or close clearly

This is a correctness feature, not generic signal buffering.

### Suspension reason categories

Initial categories:
- waiting_for_signal
- waiting_for_approval
- waiting_for_callback
- waiting_for_tool_result
- waiting_for_operator_review
- paused_by_policy
- paused_by_budget
- step_boundary
- manual_pause

### Resume condition model

Resume conditions should be richer than a boolean.
At minimum support:
- any signal from a named set
- all signals from a named set
- single explicit signal
- operator resume only
- timeout policy
- external predicate satisfied via control-plane call

Resume/data signals should be matchable against a specific **waitpoint**.
This is important for correctness when callbacks or approvals arrive early, late, or duplicated.

Potential structure:
- condition_type
- required_signal_names
- required_waitpoint_id or required_waitpoint_key if applicable
- signal_match_mode = any | all | ordered
- minimum_signal_count if needed
- timeout_behavior

### Timeout behavior

A suspended execution may define what happens when suspension times out:
- fail
- cancel
- expire
- auto-resume with timeout signal
- escalate to operator-required state

V1 can support a smaller subset, but the model should leave room for these.

### Suspension lifecycle

#### Enter suspension
From Active with a valid lease.
Persist suspension metadata, release ownership, transition execution to Suspended.

#### Receive signals
Signals may accumulate while suspended.
Each signal is evaluated against the resume condition.

Resume/data signals should normally target the current suspension via `waitpoint_id` or externally via `waitpoint_key`.
This prevents stale or duplicated callbacks from accidentally satisfying a later unrelated suspension on the same execution.

#### Resume eligibility
Once resume conditions are satisfied, the execution becomes eligible for Waiting again, or directly claimable depending on scheduling model.

#### Timeout / expiry
If timeout is reached before successful resume, timeout policy is applied.

#### Operator intervention
Operator may resume, cancel, fail, or modify suspension conditions if allowed by policy.

### Interaction with execution state

Suspension is conceptually a lifecycle state, not just a blocking reason.
That matters because:
- it is durable
- it is intentional
- it releases ownership
- it has its own control semantics

Blocking reason for a suspended execution can still be more specific, such as:
- waiting_for_approval
- waiting_for_callback
- waiting_for_signal

### Continuation model

Suspension often exists because execution needs to continue later with context.
The primitive should not prescribe language-level continuation mechanics, but it must support:
- continuation metadata
- last completed step
- resumable cursor or checkpoint reference
- signal payload access on resume

FlowFabric should persist enough continuation context for the worker runtime to resume meaningfully.

### Minimal suspension operations

- suspend_execution(execution_id, lease, suspension_spec)
- get_suspension(execution_id)
- list_suspension_signals(execution_id)
- resume_execution(execution_id, trigger)
- expire_suspension(execution_id)
- cancel_suspension(execution_id)

### Error model

Useful explicit errors:
- execution_not_active
- invalid_lease_for_suspend
- already_suspended
- invalid_resume_condition
- suspension_timeout_elapsed
- execution_not_suspended

### Open design questions

1. Should suspension and waiting-for-children share any generic blocked-state machinery, or remain distinct top-level states?
2. Should resume move to Waiting first, or allow direct reacquisition in some paths?
3. Should timeout generate a synthetic signal event for auditability?
4. How much continuation metadata should the engine own versus the worker runtime?


## Primitive 4 — Signal

### Purpose

Signal is the durable external input primitive used primarily to wake, satisfy, or annotate a waiting/suspended execution context.

Signal exists to support:
- human approval or rejection
- webhook callbacks
- external system acknowledgements
- tool results
- out-of-band control inputs
- multi-step coordination

Signal is not the same as a queue message.
A signal targets an existing execution or flow context and affects its control state.

Decision: for maintainability, FlowFabric should distinguish between:
- **resume/data signals** used for suspended or waiting execution contexts
- **explicit control operations** used for active execution control such as cancel, revoke, reroute, or operator override

This keeps the signal primitive narrow and prevents it from becoming a catch-all mutation channel.

### Definition

A signal is a durable, addressable input event delivered to a target execution or waitpoint.
The engine records the signal, applies delivery rules, and evaluates whether it changes execution eligibility or state.

### Core invariants

#### Invariant G1 — Durable receipt
Once accepted, a signal must be durably recorded before it is considered delivered.

#### Invariant G2 — Targeted delivery
A signal targets a specific execution or suspension/waitpoint scope.
It is not anonymous broadcast by default.

#### Invariant G3 — Delivery is separate from effect
A signal being stored does not automatically mean the execution resumes.
Resume depends on execution state and suspension condition rules.

#### Invariant G4 — Ordered visibility per execution
Signals for one execution should have a stable per-target order for inspection and replay of control history.

#### Invariant G5 — Dedup support
Signal delivery should support idempotency or dedup keys when external systems may retry callbacks.

### Signal record

A signal record should include:
- signal_id
- target_execution_id if applicable
- target_waitpoint_id if applicable
- target_scope
- signal_name
- payload
- payload_encoding metadata
- source_type
- source_identity
- correlation_id if any
- idempotency_key if any
- created_at
- accepted_at
- observed_effect if any

### Signal categories

Useful initial categories:
- approval
- rejection
- callback
- tool_result
- operator_control
- timeout
- custom_domain_signal

These categories are not mandatory API nouns, but they help operators reason about signals.

### Delivery model

FlowFabric should support at least:
- deliver to exact execution
- deliver to suspended execution
- deliver to specific waitpoint
- reject or redirect attempts to use resume/data signals as generic active-control commands
- buffer a resume/data signal **only if** it targets a known waitpoint

Resume/data signals should not be buffered generically by execution ID alone.
That is too dangerous once retries, reclaims, replays, and multiple suspension episodes exist.

Possible policies later:
- reject if target is not signalable
- buffer until target reaches compatible state when a known waitpoint exists
- apply to most recent open suspension only for explicitly configured compatibility modes

### Effect model

A signal may have one or more effects:
- no-op record only
- appended to signal buffer
- marks a resume condition as satisfied
- immediately moves execution back to Waiting
- updates progress / metadata
- wakes a flow coordinator

The signal primitive should record actual effect for auditability.
It should also record whether the signal matched a waitpoint, was buffered for a known pending waitpoint, was rejected as stale, or was treated as a no-op.

### Signal consumption semantics

Waitpoints are multi-signal until they close.
That means:
- a waitpoint may accept multiple signals while open
- signal idempotency still applies
- once the wait condition is satisfied and the waitpoint closes, later signals are rejected or recorded as no-op
- one-shot approval/callback UX can still be layered on top where needed

Important distinction:
- signals are delivered and recorded
- resume conditions evaluate against recorded signals
- worker runtime may later read signal payloads during continuation

FlowFabric should avoid “signal vanished because resume happened instantly” behavior.
Recorded signals should remain inspectable according to retention policy.

### Signal matching model

Signals should be matchable by:
- name
- category
- correlation_id
- source
- explicit suspension requirement
- waitpoint_id / waitpoint_key

For v1, waitpoint matching plus name-based matching is the most important combination.
That gives FlowFabric a safe way to handle early-arriving callbacks and approvals without introducing generic execution-level signal buffering.

### Minimal signal operations

- send_signal(target, signal)
- send_signal_to_waitpoint(waitpoint, signal)
- get_signal(signal_id)
- list_signals(target)
- get_pending_signals(execution_id)
- evaluate_resume_conditions(execution_id)

### Error model

Useful explicit errors:
- target_not_found
- target_not_signalable
- duplicate_signal
- invalid_signal_payload
- signal_rejected_by_policy

### Open design questions

1. Decision: in v1, resume/data signals are execution/waitpoint-scoped.
   Passive flow containers do not own waitpoints; flow-level waiting should use a coordinator execution.
2. Decision: active execution control uses explicit control operations, not generic signals.
   Signals remain focused on suspended/waiting resume-data semantics.
3. How much routing logic should exist for signals sent to a flow root versus a specific node?
4. How much compatibility buffering do we want for signals that arrive before the corresponding suspension is durably committed, if they do carry a valid waitpoint?


## Primitive 5 — Stream

### Purpose

Stream is the ordered partial-output channel attached to an **attempt**.
It exists so long-running work can emit useful output before terminal completion.

Stream exists to support:
- token streaming from model calls
- logs and progress frames
- intermediate structured output
- tool events
- operator observation
- client-side live UX

### Definition

A stream is an append-only ordered sequence of frames associated with one **attempt**.
It is distinct from final result state and distinct from engine lifecycle events.

Execution lifecycle answers "what state is the execution in?"
Attempt stream answers "what did this concrete run produce so far?"

### Why attempt-scoped stream

Decision: the true stream is **attempt-scoped**, not execution-scoped.

Rationale:
- replay, retry, and reclaim produce distinct concrete runs
- output should map cleanly to the attempt that produced it
- avoids mixing stale/partial output from failed or interrupted attempts with later successful output
- gives cleaner audit and debugging semantics

To preserve good UX, the execution API may provide an optional **merged execution stream view** that aggregates attempt streams in attempt order.

### Core invariants

#### Invariant T1 — Per-attempt order
Frames for one attempt must have a stable total order.

#### Invariant T2 — Append-only
Frames are appended, not mutated in place.
Any redaction or truncation policy should be explicit and auditable.

#### Invariant T3 — Separate from terminal result
Final result metadata must remain separate from intermediate stream frames.

#### Invariant T4 — Live and replayable
Consumers should be able to read from the beginning, from an offset, or tail the live attempt stream according to retention policy.

#### Invariant T5 — Multi-type frames
Stream must support more than text tokens.
It should support logs, structured progress, artifacts, and tool events.

### Stream record and metadata

Each attempt stream should expose:
- stream_id
- execution_id
- attempt_id
- attempt_index
- created_at
- last_offset or sequence
- closed_at if terminally closed
- retention policy
- durability mode

Each frame should expose:
- offset / sequence number
- frame_type
- timestamp
- payload
- payload_encoding metadata
- optional correlation metadata

### Frame types

Initial frame types:
- token
- text_chunk
- log
- progress
- usage_update
- artifact_reference
- tool_event
- structured_json
- warning
- debug

### Stream lifecycle

#### Open
A stream becomes available once an attempt starts producing frames, or eagerly at attempt creation if desired by API.

#### Append
A valid producer appends frames in order.
This is normally the active owner of the attempt.

#### Tail / subscribe
Clients can read from an offset or subscribe to new frames.

#### Close
The stream may close automatically on terminal attempt outcome, or remain queryable but no longer appendable.

#### Retain / trim
Retention policy determines how long stream data remains available.

### Execution-level merged stream view

The execution API may expose a merged stream view that:
- concatenates or groups frames by attempt order
- preserves attempt attribution on each frame
- makes simple “watch this execution” UX possible

This merged view is a convenience surface, not the deepest source of truth.

### Durability modes

This needs an explicit model because streaming can become expensive.

Possible modes:
- durable_full
- durable_summary
- best_effort_live

For v1, FlowFabric can choose one default, but the spec should acknowledge that stream durability is a policy decision, not a law of nature.

### Ownership and append rules

By default, appending frames should require a valid active lease or privileged system context.
This prevents stale workers from continuing to emit output after reclaim.

Possible exception:
- control-plane or operator annotations may append outside normal worker ownership with a distinct source marker.

### Relationship to engine events

Attempt stream is not the same as the engine event feed.

- event feed: state transitions, scheduling, ownership, control actions
- stream: execution-produced content and intermediate artifacts

Keeping them separate prevents observability confusion.

### Minimal stream operations

- open_attempt_stream(attempt_id)
- append_frame(attempt_id, lease, frame)
- read_attempt_stream(attempt_id, from_offset)
- tail_attempt_stream(attempt_id)
- read_execution_stream(execution_id, merge_mode)
- close_attempt_stream(attempt_id)
- get_stream_info(attempt_id)

### Error model

Useful explicit errors:
- stream_not_found
- stream_closed
- stale_owner_cannot_append
- invalid_frame_type
- invalid_offset
- retention_limit_exceeded

### Open design questions

1. Should streams be created eagerly for every attempt or lazily on first append?
2. What should the default durability mode be for v1?
3. Should large artifacts live in-stream, or only as references?
4. What merge modes should the execution-level stream view support in v1?

## Primitive 6 — Flow

Decision: **Flow is a real coordination container, with an optional root execution.**

This means Flow is not defined as “a rooted execution tree only.” A root execution is common and useful, but it is not mandatory for the existence of a flow.

### Purpose

Flow is the durable coordination primitive for multiple related executions.

It exists to support:
- parent-child trees
- ordered chains and stage pipelines
- fan-out / fan-in patterns
- DAG-style dependency graphs
- dynamic child spawning
- grouped failure and budget policy
- aggregate inspection across related execution

Flow is not just a convenience wrapper.
It is the coordination scope that ties many execution records into one unit of progress and policy.

### Definition

A flow is a durable coordination container that groups executions and defines dependency, aggregation, and policy relationships among them.

Decision: the flow container itself remains **passive**.
If a flow needs to wait for external input, suspend, or resume, that waiting should live on a real coordinator execution inside the flow, not on the passive flow container itself.

A flow may contain:
- zero or one root execution
- zero or more member executions
- directed dependency edges
- flow-scoped policy such as failure mode, budget attachments, or default routing hints
- aggregate summaries and completion logic independent of any single execution record

### Core invariants

#### Invariant F1 — Stable flow identity
A flow has a stable flow ID throughout its lifetime.
Executions may be added dynamically, but they remain associated with the same coordination object.

#### Invariant F2 — Execution belongs to coordination scope explicitly
An execution belongs to a flow only if that membership is explicitly recorded.
No implicit “same queue means same flow” shortcuts.

#### Invariant F3 — Dependency-driven eligibility
When a flow uses dependency edges, execution eligibility must be determined by explicit dependency satisfaction, not ad hoc worker behavior.

#### Invariant F4 — Coordination separate from execution identity
Flow coordinates executions but does not replace them.
Each execution keeps its own lifecycle, output stream, lease, and retry semantics.

#### Invariant F5 — Flow policy is visible
Failure policy, budget policy, and dependency semantics must be inspectable at the flow level.

### Topology model

FlowFabric should support these topology classes:

#### Tree
Each node has at most one parent execution and edges form a rooted hierarchy.
Useful for parent-child work distribution.

In v1, rooted tree flows should be the most common and best-supported path, even though the flow object itself is not required to be identical to the root execution.

#### Chain
Executions run in ordered stages where later stages depend on earlier completion.
Useful for pipelines.

#### Fan-out / fan-in
One coordinator execution spawns many children and later joins their results.

#### DAG
A node may depend on multiple predecessors.
Used for richer orchestration where eligibility is based on multiple upstream conditions.

#### Dynamic expansion
New nodes or edges may be added during runtime, subject to validation.

### Flow object fields

#### Identity
- flow_id
- optional flow_name / kind
- created_at
- created_by
- tenant / namespace scope if applicable

#### Structure
- root_execution_id if applicable
- has_root_execution flag
- topology_kind
- node_count summary
- edge_count summary
- dynamic_expansion_enabled flag or policy

#### Policy
- failure_policy
- completion_policy
- retry_propagation policy if any
- cancellation propagation policy
- budget attachments
- default routing hints if any

#### Summary / derived state
- active_count
- waiting_count
- suspended_count
- completed_count
- failed_count
- blocked_count
- unresolved_dependency_count
- aggregate usage summary

### Dependency model

Each dependency edge should capture:
- upstream execution ID
- downstream execution ID
- dependency kind
- satisfaction condition
- optional data-passing / aggregation behavior reference

Useful dependency kinds:
- success-only dependency
- completion dependency
- signal-like dependency
- quorum / count-based dependency later

For v1, the default and most important dependency kind should be **success-only**.
That means a downstream node becomes eligible only when its required predecessors completed successfully.

V1 scope decision:
- support `all required predecessors succeeded`
- defer `any-of`, quorum, threshold, and other partial-join dependency conditions to a later version

The model should still leave room for these richer dependency kinds later without breaking edge representation.

### Eligibility rules

An execution inside a flow becomes eligible when:
- it is in a runnable lifecycle state
- all required inbound dependencies are satisfied
- flow-level failure policy does not block it
- budget or route policy does not block it

When dependencies remain unresolved, execution should sit in WaitingChildren or another explicit blocked condition.

### Failure policies

Flow should support explicit policy rather than hard-coded behavior.

Useful initial policies:
- fail_fast: any critical child failure fails or blocks the whole flow
- isolate_failures: failing node affects only the dependent subgraph unless other policy says otherwise
- collect_all: allow all branches to finish before aggregate decision
- coordinator_decides: root/coordinator execution handles downstream policy

Decision: for DAG behavior, the default should be **dependency-local propagation**, not global fail-fast.
That means:
- a failed node affects only nodes that depend on it
- unrelated branches may continue
- downstream nodes whose required dependencies can no longer be satisfied should become terminal `skipped`
- the overall flow may still derive a failed status depending on aggregate policy

V1 can support a smaller subset, but the policy surface should be acknowledged.

### Completion model

Flow completion should be derived from execution outcomes plus flow policy.
Possible completion conditions:
- root execution completed and no unresolved children remain
- all executions reached terminal state
- aggregate success threshold achieved
- coordinator declared flow complete

For DAGs specifically, completion semantics should account for terminal `skipped` nodes caused by failed required dependencies.
A flow should not remain semantically open forever just because some descendants became impossible to run.

Flow should expose a derived state summary, but execution remains the primary lifecycle primitive.

### Dynamic child spawning

Flow must support adding child executions at runtime.
This is required for agent loops, recursive decomposition, and adaptive fan-out.

The engine should validate:
- new membership
- dependency edges
- no illegal topology mutations for current policy
- optional cycle checks for DAG mode

### Cancellation and propagation

Flow-level cancellation should support policy-controlled propagation:
- cancel all descendants
- cancel only unscheduled descendants
- mark flow cancelled but let active children finish
- coordinator decides

### Waiting and signaling model

The passive flow container does not own waitpoints.
If flow-level orchestration needs to pause and later resume, that behavior should be modeled through a coordinator execution that belongs to the flow.

Rationale:
- one suspension/waitpoint model instead of two
- one lease/reclaim/replay model instead of duplicating it at flow level
- simpler correctness and audit semantics
- rootless flows may still exist as passive coordination containers

### Inspection surface

Flow inspection should answer:
- which executions belong here
- which are active / blocked / failed / suspended
- what dependencies remain unresolved
- why the flow is not finished
- what budget has been consumed

### Minimal flow operations

- create_flow(flow_spec)
- add_execution_to_flow(flow_id, execution_spec)
- add_dependency(flow_id, edge)
- resolve_dependency(flow_id, edge or event)
- get_flow(flow_id)
- get_flow_graph(flow_id)
- get_flow_summary(flow_id)
- cancel_flow(flow_id, policy)
- attach_budget_to_flow(flow_id, budget_id)

### Error model

Useful explicit errors:
- flow_not_found
- execution_not_in_flow
- duplicate_flow_membership_entry
- invalid_dependency
- cycle_detected
- flow_mutation_not_allowed
- flow_policy_violation

### Open design questions

1. Decision: a flow is a real coordination container and may exist without a root execution.
   A root execution is optional, not mandatory.
2. Do we want true DAG support in v1, or tree + dynamic children first?
3. Should flow state be purely derived, or should some flow-level transitions be stored explicitly?
4. How much dependency metadata should be colocated on execution records versus separate flow indexes?


## Primitive 7 — Budget

Decision: **Budget, quota, and rate limit are separate user-facing policy families.**

They may share limiting/enforcement machinery under the hood, but they must remain distinct in the public model because:
- they govern different resources
- users think about them differently
- they need different configuration surfaces
- they often want different breach behavior and recovery policy

Examples:
- budget breach may close, suspend, or degrade work
- quota breach may defer or retry in a later window
- rate-limit breach may jitter, back off, or queue for later admission

### Purpose

Budget is the resource-governance primitive for FlowFabric.

It exists to support:
- token limits
- cost limits
- request limits
- execution count limits
- grouped usage control across flows or tenants
- policy-driven pause or failure when limits are exceeded

Budget is not just observability.
It is an enforcement object.

### Definition

A budget is a durable, inspectable resource allowance attached to one or more execution scopes.
A budget tracks usage against limits and defines what the engine should do when usage approaches or exceeds those limits.

### Core invariants

#### Invariant B1 — Explicit scope
Every budget has an explicit scope.
Examples:
- execution-scoped
- flow-scoped
- lane-scoped
- tenant-scoped

#### Invariant B2 — Usage and enforcement are coupled
If the engine claims to enforce a budget, the usage record and limit check must be applied atomically enough to avoid obvious overshoot races.

#### Invariant B3 — Breach policy is explicit
A limit breach must not be ambiguous.
Each budget defines what happens on breach.

#### Invariant B4 — Inspectability
At any moment, operators should be able to inspect:
- limit values
- current consumption
- remaining allowance
- recent breaches or near-breaches
- attached scope

### Budget dimensions

Budget dimensions should be **extensible**, not a closed enum.

FlowFabric should support a typed resource/accounting map with a small set of well-known built-ins and room for future dimensions.

Examples of built-ins:
- total_input_tokens
- total_output_tokens
- total_tokens
- total_thinking_tokens
- total_cost

Examples of future/extensible dimensions:
- latency_ms
- effort_units
- provider_specific_reasoning_units
- custom model/runtime resource dimensions

Budget should model **consumable allowance**, not generic pacing.
Execution-count caps and time-window request caps belong more naturally to quota/rate-limit policy unless there is a strong product reason to treat them as budget dimensions.

### Budget fields

#### Identity and scope
- budget_id
- scope_type
- scope_id
- created_at
- created_by

#### Limit definitions
- hard_limits map
- soft_limits map if supported
- currency / pricing metadata if cost is used
- reset policy if recurring

#### Usage state
- current_usage map
- last_updated_at
- breach_count
- near_limit_count if tracked

#### Enforcement policy
- on_soft_limit behavior
- on_hard_limit behavior
- enforcement_mode
- optional escalation target

Useful budget enforcement actions:
- suspend execution
- fail execution
- deny child spawning
- emit warning only
- reroute to cheaper fallback tier
- close or stop further work in the affected scope

### Budget attachment model

A budget may be attached to:
- one execution
- one flow
- one submission lane
- one tenant or namespace

Multiple budgets may apply simultaneously.
Example:
- tenant monthly cost budget
- flow total token budget
- single execution max cost guardrail

The engine should preserve enough metadata to explain which budget blocked progress.

### Usage reporting model

Usage should be reported through explicit operations such as:
- token usage increment
- thinking-token usage increment
- cost usage increment
- extensible resource-dimension usage increment

The engine should support:
- post-hoc charge after usage is known
- optional preflight estimate check later

### Enforcement timing

Budget checks may happen at several points:
- before claim
- before child spawn
- during active execution on usage report
- before fallback escalation
- before resume from suspension

The spec should acknowledge these distinct enforcement points.

### Blocking and breach reasons

When budget prevents progress, execution should expose a specific blocking reason such as:
- waiting_for_budget
- paused_by_budget
- failed_budget_exceeded

### Minimal budget operations

- create_budget(spec)
- attach_budget(scope, budget_id)
- get_budget(budget_id)
- get_budget_usage(budget_id)
- report_usage(scope, usage_delta)
- check_budget(scope)
- override_budget(budget_id, operator_action)

### Error model

Useful explicit errors:
- budget_not_found
- invalid_budget_scope
- budget_exceeded
- budget_attach_conflict
- unsupported_usage_dimension

### Open design questions

1. Do we want only post-hoc usage reporting in v1, or also reservation / precharge semantics?
2. Should cost be reported purely by workers, or can routing/fallback policy inject expected cost metadata?
3. Which built-in budget dimensions should receive first-class default UX in v1, even though the model itself is extensible?
4. Should flow budget enforcement block child creation, child start, or both?

## Primitive 7.5 — Quota and rate-limit policy

### Purpose

Quota and rate-limit policy govern **admission and pacing**, not consumable budget.

They exist to support:
- requests per minute / hour / day
- tokens per minute
- active concurrency caps
- tenant throttling
- fairness windows
- jittered retry / deferred admission

### Why separate from budget

Budget answers: how much total allowance may this scope consume?

Quota / rate limit answer: how fast or how often may this scope proceed?

These may be enforced using similar internal mechanisms, but they are not the same user concept and should not collapse into one public API.

### Core invariants

#### Invariant QL1 — Distinct public policy surface
Users must be able to configure quota/rate-limit policy separately from budget policy.

#### Invariant QL2 — Distinct breach behavior
Quota/rate-limit breaches may trigger different outcomes than budget breaches.
Examples:
- retry later
- defer to next window
- jitter backoff
- temporary throttle

#### Invariant QL3 — Explainable admission failure
If execution cannot proceed because of quota or rate limit, the engine must expose that reason clearly.

### Policy dimensions

Useful dimensions:
- requests_per_window
- tokens_per_minute
- active_concurrency_cap
- per-tenant concurrency cap
- per-lane throughput cap

### Enforcement behavior examples

Useful actions:
- deny claim now and retry later
- move to delayed eligibility
- jitter next eligibility time
- throttle by lane or tenant
- emit warning / metrics

### Relationship to public state

Quota/rate-limit blocks typically render as:
- public_state = rate_limited
- blocking_reason carrying the specific cause

Examples:
- waiting_for_quota
- waiting_for_tokens_per_minute
- waiting_for_concurrency_slot

### Minimal quota/rate-limit operations

- create_quota_policy(spec)
- attach_quota_policy(scope, policy_id)
- check_admission(scope)
- record_admission(scope, usage_delta)
- get_quota_policy(policy_id)
- get_quota_status(scope)

### Open design questions

1. Which pacing controls are v1-mandatory: requests/window, tokens/minute, concurrency cap?
2. Should concurrency caps live under quota policy or separate admission policy naming?
3. Do we want soft-throttle versus hard-deny distinction in v1?


## Primitive 8 — Capability

### Purpose

Capability is the descriptive matching primitive between execution requirements and worker traits.

It exists to support:
- model-specific worker pools
- isolation requirements
- sandbox or trust requirements
- regional / locality affinity
- hardware or performance class targeting
- tenant or compliance segregation

Capability is not authorization by itself.
It is a routing and eligibility descriptor.

### Definition

A capability is a typed attribute or label advertised by a worker or required by an execution.
Capability matching determines whether a worker is eligible to claim a particular execution.

### Core invariants

#### Invariant C1 — Requirements must be explicit
If an execution requires a property, that requirement must be encoded explicitly, not inferred from queue name alone.

#### Invariant C2 — Ineligible workers must not claim
A worker lacking required capability must not successfully claim the execution.

#### Invariant C3 — Capability view is inspectable
Operators should be able to explain why an execution is waiting for a capable worker.

#### Invariant C4 — Matching must be deterministic enough to debug
Given the same worker capability snapshot and execution requirement set, matching results should be explainable and stable enough for diagnosis.

### Capability categories

Useful initial categories:
- runtime_language
- provider_access
- model_family
- model_tier
- sandbox_level
- network_access_class
- region / zone
- hardware_class
- tenant_affinity
- compliance_class
- tool_access_set

### Worker capability record

A worker capability record should include:
- worker_id
- worker_instance_id
- capability set / attributes
- registered_at
- last_seen_at
- optional health / status snapshot
- optional capacity hints

### Execution requirement record

An execution requirement record should include:
- required capabilities
- preferred capabilities
- forbidden capabilities
- minimum isolation or trust level
- optional locality / tenancy hints

### Matching semantics

Initial matching should support:
- required set must be satisfied
- forbidden set must not be present
- preferences influence route selection but are not mandatory

Later we may add scoring or weighted matching.

### Capability freshness

Capabilities are not purely static.
Workers may change health, current load, or temporary availability.

The primitive should distinguish between:
- relatively static capability traits
- dynamic capacity / health signals

Capability itself describes what a worker can do.
Dynamic availability is better handled by route selection and worker status.

### Minimal capability operations

- register_worker_capabilities(worker_id, capability_record)
- update_worker_capabilities(worker_id, capability_record)
- get_worker_capabilities(worker_id)
- set_execution_requirements(execution_id, requirement_record)
- get_execution_requirements(execution_id)
- explain_capability_mismatch(execution_id)

### Error model

Useful explicit errors:
- capability_record_not_found
- invalid_capability_definition
- execution_requirements_invalid
- no_capable_worker_available

### Open design questions

1. Should capability be strongly typed from day one, or partly label-based for flexibility?
2. How much of provider/model routing should be capability versus higher-level policy?
3. Do we want capability inheritance at lane or flow level?
4. Should temporary worker health be represented outside capability entirely?


## Primitive 9 — Route

### Purpose

Route is the placement decision primitive that determines where an eligible execution may run.

It exists to combine:
- capability matching
- worker availability
- fairness
- locality
- tenant isolation
- quota and budget constraints
- routing preferences

Route is where execution intent becomes concrete placement.

### Definition

A route is the resolved placement plan for an execution at claim time or before claim time.
It may point to:
- a lane or queue-like ingress partition
- a worker pool
- a region or locality
- a capability class
- a specific backend partition or shard if needed internally

### Core invariants

#### Invariant R1 — Route must respect requirements
A route may never select a destination that violates hard execution requirements.

#### Invariant R2 — Route must be inspectable
The system must be able to explain why an execution was routed or why it remains unroutable.

#### Invariant R3 — Route may change over time
Routing is not necessarily immutable.
An execution can be rerouted before claim, or after suspension/retry, as long as transitions are explicit.

#### Invariant R4 — Routing and ownership are separate
A route suggests or determines where execution may be claimed.
A lease determines who currently owns it.

### Route inputs

Route resolution may consider:
- execution requirements
- preferred capabilities
- flow-level routing hints
- lane defaults
- tenant or compliance policy
- locality preferences
- current worker availability
- fairness and anti-starvation policy
- quota and budget signals
- fallback position

### Route outputs

A resolved route should expose:
- route_id or route snapshot ID if persisted
- selected lane / pool / class
- selected locality if any
- selected capability profile or worker group
- route_reason summary
- route version / timestamp

### Route outcomes

Possible route outcomes:
- routable_now
- waiting_for_capable_worker
- waiting_for_capacity
- waiting_for_locality_match
- waiting_for_quota
- reroute_required
- unroutable_by_policy

These outcomes map naturally into blocking reasons.

### Routing modes

Useful initial routing modes:
- direct lane routing
- capability-based pool routing
- locality-aware routing
- fallback rerouting after failure or budget pressure

### Reroute model

Reroute should be allowed when:
- execution is not actively leased
- retry policy changes target
- fallback policy advances model/provider tier
- operator explicitly reroutes
- locality or worker availability conditions changed

The engine should record reroute lineage for auditability.

### Minimal route operations

- resolve_route(execution_id)
- get_route(execution_id)
- reroute_execution(execution_id, reason)
- explain_routing(execution_id)
- list_route_candidates(execution_id) (optional / diagnostic)

### Error model

Useful explicit errors:
- route_not_found
- unroutable_execution
- reroute_not_allowed
- no_capacity_available
- no_matching_route

### Open design questions

1. Should route be a persisted object or a derived snapshot computed on demand?
2. How much fairness logic belongs inside route resolution versus separate scheduling policy?
3. Do we want explicit worker-pool abstraction in v1, or just lanes plus capability filtering?
4. Should fallback progression be modeled as route change or execution policy change that influences route?


## Primitive 10 — Submission / Queue facade

### Purpose

Submission is the external ingress primitive.
The queue facade is the compatibility and ergonomics layer built on top of execution creation.

This exists because many users still think in producer / queue / worker terms, and that is fine.
But the engine should not let that facade redefine the underlying model.

### Definition

A queue facade is a named submission lane with default policies that materializes executions.

It may provide familiar concepts such as:
- producer
- queue
- worker
- delay
- retry
- priority
- pause/resume
- counts
- request/reply

But under the hood, each submitted item becomes an execution with full FlowFabric semantics.

### Core invariants

#### Invariant Q1 — Queue does not replace execution
Every queued item maps to an execution.
There is no parallel shadow “job model” with different semantics.

#### Invariant Q2 — Queue defaults are policy templates
Queue or lane configuration should provide defaults for execution policy, not separate state rules.

#### Invariant Q3 — Queue operations cannot bypass core invariants
Enqueue, pause, retry, resume, and count APIs must still respect lease, suspension, flow, routing, and budget semantics.

### Lane / queue concept

The facade likely needs a named ingress concept.
Possible public names:
- queue
- lane
- class
- channel

My current preference for the engine internals is **lane**.
It is less constraining than queue, while still understandable.
A public compatibility API can still expose `Queue` if needed.

### Lane fields

- lane_id or queue_name
- default retry policy
- default priority
- default routing hints
- default capability requirements
- default budget attachments
- scheduling policy
- paused / intake state
- visibility / tenancy scope

### Submission modes

Initial modes:
- fire-and-forget
- delayed
- scheduled
- request/reply wait
- bulk submission
- deduplicated submission

### Queue-facing operations

- enqueue(lane, payload, opts)
- enqueue_delayed(lane, payload, at_or_after)
- enqueue_bulk(lane, items)
- enqueue_and_wait(lane, payload, wait_policy)
- pause_lane(lane)
- resume_lane(lane)
- get_lane_counts(lane)
- get_lane_metrics(lane)

### Queue worker facade

A queue-oriented worker API may exist for familiarity.
It should be a thin wrapper around:
- claim eligible execution from compatible lanes
- obtain lease
- run processor
- report usage / stream / progress
- complete / fail / suspend

### Why keep the facade

Because it lowers adoption friction.
Teams migrating from queue systems should not need to think in graph-first or orchestration-first terms immediately.

### Why keep it as a facade only

Because if queue becomes the core identity again, FlowFabric loses its main differentiation.

### Open design questions

1. Should the public API say Queue or Lane by default?
2. Do we want first-class request/reply in v1, or just a helper over execution waiters?
3. How much BullMQ-style migration compatibility is strategically worth carrying?
4. Should scheduled and recurring execution live under the queue facade or as top-level execution submission features?



---

# Cross-cutting spec — Object model, state model, operation semantics, and V1 scope

## 1. Object model

This section defines which objects are first-class in FlowFabric, which are derived, and which are implementation detail.

The point is to avoid two classic mistakes:
- collapsing everything into a single overgrown execution record
- inventing too many first-class objects and making the system impossible to reason about

### 1.1 First-class durable objects

These objects must exist durably in the engine model.

#### Execution
The primary unit of controlled work.
Everything else either constrains, coordinates, owns, observes, or routes execution.

#### Flow
The durable coordination scope for multiple related executions.
Needed for dependency graphs, grouped policy, grouped budget, and aggregate inspection.

#### Lease
The bounded ownership contract for active execution.
Needed for correctness, stale-owner fencing, reclaim, and crash recovery.

#### Suspension
The durable wait record for intentionally paused execution.
Needed for signal-based resume, human approval, callback waits, and step continuation.

#### Signal
The durable external input record that targets an execution or flow context.
Needed for auditability and deterministic resume behavior.

#### Stream
The append-only ordered output channel for attempt-emitted content, with optional merged execution-level views.
Needed for partial output, live UX, and replayable intermediate results.

#### Budget
The durable consumable-allowance object.
Needed for grouped cost/token/thinking-budget enforcement and operator visibility.

#### Quota / rate-limit policy
The durable admission and pacing control object.
Needed for request caps, per-window limits, throughput shaping, and fairness-oriented gating.

#### Lane
The named submission and policy surface.
This is the queue-compatible ingress object, but it should remain a facade over execution semantics rather than a competing core model.

#### Worker registration / capability record
The durable or semi-durable identity and capability advertisement of workers.
Needed for routing, inspection, and explainability.

### 1.2 Derived objects / projections

These objects are important, but they should usually be treated as derived views rather than primary truth.

#### Execution summary
Compact view of lifecycle, blocking reason, usage, lease status, and recent progress.
Useful for dashboards and list APIs.

#### Flow summary
Aggregate view of member states, unresolved dependencies, budget usage, and overall completion condition.

#### Route explanation / route snapshot
Derived explanation of why an execution is routed somewhere or is currently unroutable.
May be persisted for audit, but routing truth should come from inputs plus current state.

#### Lane metrics
Counts, throughput, backlog, retries, suspensions, average latency, etc.

#### Worker health summary
Liveness, current load, current claims, capability snapshot, and recent errors.

#### Execution history summary
A compact view of important state transitions without requiring full raw event replay.

### 1.3 Ephemeral / implementation-detail objects

These may exist in implementation, but they should not be treated as durable product-level primitives.

#### Live subscriptions
Tail-stream consumers, watchers, SSE sessions, websocket listeners.

#### In-flight processor context
Language runtime state, local continuation objects, open sockets, in-memory buffers.

#### Scheduler tick bookkeeping
Timers, promotion loop cursors, reclaim scan cursors, background task state.

#### Route candidate caches
Transient optimization structures for routing.

### 1.4 Recommended relationships

At minimum, the durable relationship graph should look like this:

- a **Lane** accepts submissions and applies default policy to create **Execution**
- an **Execution** may belong to zero or one **Flow**
- an **Execution** may have zero or one current **Lease**
- an **Execution** may have zero or one active **Suspension**
- an **Execution** may have many **Signals** over time
- an **Execution** may have one or more attempt-scoped **Streams** and may expose a merged execution stream view
- an **Execution** or **Flow** may attach to one or more **Budgets**
- a **Worker registration** advertises **Capabilities** used by routing and claiming

### 1.5 Identity rules

#### Execution identity
Execution ID is stable for the life of the logical execution.
Attempts and fallbacks do not create a new logical execution by default.

#### Attempt identity
Each execution attempt should have a stable attempt index and, if helpful, a separate attempt ID for tracing.

#### Flow identity
Flow ID is stable even as members are added dynamically.

#### Lease identity
Lease ID changes on every successful acquisition or reclaim.
Lease epoch / fencing token must be monotonic.

#### Suspension identity
A new suspension record should exist for each distinct suspension episode.
One execution may accumulate many suspension episodes over its full life.

#### Signal identity
Each accepted signal gets its own signal ID.
Idempotency keys prevent duplicate external delivery from creating duplicated logical signals.

### 1.6 Object ownership boundaries

These boundaries are important for RFC work later.

#### Engine-owned
- execution lifecycle
- lease correctness
- suspension records
- signal durability
- stream order and retention
- flow dependency bookkeeping
- budget enforcement bookkeeping
- lane intake state

#### Worker-runtime-owned but engine-observable
- processor-local continuation state
- language-specific tool handles
- detailed in-memory context needed to resume user code

#### Product/control-plane-owned
- business workflow truth
- user-facing approval entities
- prompt registry
- memory and retrieval stores
- chat/session model
- eval product state

FlowFabric should make these boundaries explicit so it does not become a kitchen-sink orchestration product.

---

## 2. State model

The state model should be expressed as a **constrained orthogonal state vector**.

That means:
- execution state is not a single giant enum
- different dimensions answer different questions
- not every combination is valid
- the engine, not clients, owns the derivation of user-facing state labels

This is the right fit for FlowFabric because it is an execution engine, not just a queue.

A queue can get away with a flatter state model.
An execution engine with leases, suspensions, signals, flows, budgets, retries, and routing cannot.

### 2.1 Why orthogonal state

Orthogonal state is useful because the engine needs to answer several distinct questions at once:
- what phase is this execution in?
- who owns it right now?
- is it eligible to run?
- if not, why not?
- if it ended, how did it end?
- what is the current attempt doing?

Trying to force all of that into a single enum causes state explosion and poor invariants.

### 2.2 State vector model

Each execution should be represented by a state vector with the following dimensions.

#### A. Lifecycle phase
This is the anchored phase dimension.
It answers: what major phase of existence is this execution in?

Suggested values:
- submitted
- runnable
- active
- suspended
- terminal

This is the closest thing to a canonical lifecycle, but it is not enough on its own.
It must be combined with the other dimensions below.

#### B. Ownership state
This answers: who, if anyone, is currently allowed to mutate active execution state?

Suggested values:
- unowned
- leased
- lease_expired_reclaimable
- lease_revoked

#### C. Eligibility state
This answers: can the execution be claimed for work right now?

Suggested values:
- eligible_now
- not_eligible_until_time
- blocked_by_dependencies
- blocked_by_budget
- blocked_by_quota
- blocked_by_route
- blocked_by_lane_state
- blocked_by_operator

This dimension is especially important because many user-visible “states” are really eligibility conditions, not lifecycle phases.

#### D. Blocking reason
This answers: what is the most specific current explanation for lack of forward progress?

Suggested values:
- none
- waiting_for_worker
- waiting_for_retry_backoff
- waiting_for_signal
- waiting_for_approval
- waiting_for_callback
- waiting_for_tool_result
- waiting_for_children
- waiting_for_budget
- waiting_for_quota
- waiting_for_capable_worker
- waiting_for_locality_match
- paused_by_operator
- paused_by_policy

Blocking reason is explanatory. It should not replace the more structural dimensions.

#### E. Terminal outcome
This answers: if the execution is terminal, how did it end?

Suggested values:
- none
- success
- failed
- cancelled
- expired
- skipped

#### F. Attempt state
This answers: what is happening at the concrete run-attempt layer?

Suggested values:
- none
- pending_first_attempt
- running_attempt
- attempt_interrupted
- pending_retry_attempt
- pending_replay_attempt
- attempt_terminal

This dimension matters for retries, reclaim, fallback progression, and deep inspection.

### 2.3 Public state is derived, not guessed by clients

Even though the engine stores and reasons about a state vector, users should still get a stable, human-readable `public_state` field.

The engine should derive it centrally.
Clients must not invent their own mapping.

Suggested public states:
- waiting
- delayed
- rate_limited
- waiting_children
- active
- suspended
- completed
- failed
- cancelled
- expired
- skipped

These are not the deepest truth.
They are the stable operator-facing rendering of the deeper state model.

### 2.4 Why a public state is still necessary

Orthogonal state is correct for the engine.
It is not what most users want to stare at every day.

Users want:
- one obvious state label
- plus explanation fields when needed

So the right model is:
- structured orthogonal internals
- stable `public_state`
- drill-down fields for debugging and control planes

### 2.5 Example state vectors

#### Example: ready and waiting for a worker
- lifecycle_phase = runnable
- ownership_state = unowned
- eligibility_state = eligible_now
- blocking_reason = waiting_for_worker
- terminal_outcome = none
- attempt_state = pending_first_attempt
- public_state = waiting

#### Example: delayed retry backoff
- lifecycle_phase = runnable
- ownership_state = unowned
- eligibility_state = not_eligible_until_time
- blocking_reason = waiting_for_retry_backoff
- terminal_outcome = none
- attempt_state = pending_retry_attempt
- public_state = delayed

#### Example: waiting on children
- lifecycle_phase = runnable
- ownership_state = unowned
- eligibility_state = blocked_by_dependencies
- blocking_reason = waiting_for_children
- terminal_outcome = none
- attempt_state = none or pending_retry_attempt depending on design
- public_state = waiting_children

#### Example: actively running with valid lease
- lifecycle_phase = active
- ownership_state = leased
- eligibility_state = blocked_by_operator? no; effectively not claimable by others
- blocking_reason = none
- terminal_outcome = none
- attempt_state = running_attempt
- public_state = active

#### Example: worker crashed and execution is reclaimable
- lifecycle_phase = active
- ownership_state = lease_expired_reclaimable
- eligibility_state = blocked_by_route or blocked until reclaim path; implementation-defined
- blocking_reason = waiting_for_worker
- terminal_outcome = none
- attempt_state = attempt_interrupted
- public_state = active

#### Example: reclaimed after worker crash
- same logical execution_id
- previous attempt marked interrupted/reclaimed
- new attempt created with incremented attempt_index
- new lease issued with higher fencing token
- public_state returns to active or waiting depending on claim timing

#### Example: suspended for approval
- lifecycle_phase = suspended
- ownership_state = unowned
- eligibility_state = blocked_by_operator or blocked until signal, depending on taxonomy
- blocking_reason = waiting_for_approval
- terminal_outcome = none
- attempt_state = attempt_interrupted
- public_state = suspended

#### Example: terminal success
- lifecycle_phase = terminal
- ownership_state = unowned
- eligibility_state = blocked_by_operator? no; terminal implies non-eligible
- blocking_reason = none
- terminal_outcome = success
- attempt_state = attempt_terminal
- public_state = completed

### 2.6 Validity constraints

Orthogonal dimensions do **not** mean all combinations are legal.
The engine must enforce a validity matrix.

Examples of invalid combinations:
- lifecycle_phase = terminal AND ownership_state = leased
- lifecycle_phase = suspended AND ownership_state = leased
- terminal_outcome != none AND lifecycle_phase != terminal
- lifecycle_phase = active AND attempt_state = pending_first_attempt
- eligibility_state = eligible_now AND lifecycle_phase = terminal
- public_state = completed AND terminal_outcome != success

Examples of valid but subtle combinations:
- lifecycle_phase = active AND ownership_state = lease_expired_reclaimable
- lifecycle_phase = runnable AND blocking_reason = waiting_for_children
- lifecycle_phase = runnable AND eligibility_state = not_eligible_until_time

The exact validity matrix should become its own RFC appendix or dedicated table.

### 2.7 Recommended layering

State should be understood in layers.

#### Layer 1 — Structural phase
Lifecycle phase.
This is the broadest anchored dimension.

#### Layer 2 — Ownership correctness
Ownership state.
This tells who may mutate it right now.

#### Layer 3 — Admission / scheduling
Eligibility state.
This tells whether it may run now.

#### Layer 4 — Human explanation
Blocking reason.
This tells why it is not progressing.

#### Layer 5 — Outcome
Terminal outcome.
This tells how it ended.

#### Layer 6 — Concrete run state
Attempt state.
This tells what is happening at the attempt layer.

#### Layer 7 — Public rendering
Public state.
This is the engine-supplied user-facing state label.

### 2.8 Public state derivation principles

The engine should define deterministic derivation rules from the state vector to `public_state`.

Guiding principles:

#### Principle D1 — terminal outcome dominates
If lifecycle_phase = terminal, public_state must be one of:
- completed
- failed
- cancelled
- expired
- skipped
based on terminal_outcome.

#### Principle D2 — suspended is explicit
If lifecycle_phase = suspended, public_state = suspended.
Suspension should not be hidden as just another blocked runnable state.

#### Principle D3 — active dominates non-terminal ownership nuance
If lifecycle_phase = active, public_state = active, even if ownership_state indicates reclaimable trouble.
The deeper ownership field explains the nuance.

#### Principle D4 — runnable states render by eligibility/blocking reason
If lifecycle_phase = runnable, public_state is derived mainly from eligibility_state and blocking_reason.
Examples:
- not_eligible_until_time -> delayed
- blocked_by_dependencies + waiting_for_children -> waiting_children
- blocked_by_budget / blocked_by_quota -> rate_limited or another explicit blocked public state depending on taxonomy
- eligible_now -> waiting

#### Principle D5 — submitted usually collapses to waiting-like views unless exposed intentionally
If `submitted` exists only briefly, it may render as waiting or not be externally exposed at all.
This is a product/API choice.

### 2.9 Public state taxonomy

Decision: use **specific public states**, not a single broad `blocked` umbrella.

Chosen public states:
- waiting
- delayed
- rate_limited
- waiting_children
- active
- suspended
- completed
- failed
- cancelled
- expired

Rationale:
- users and operators want immediately meaningful state labels
- queue-compatible and dashboard-compatible UX is simpler
- downstream crates can still rely on the deeper orthogonal state vector
- `blocking_reason` remains available for precise explanation underneath the public label

This means public state is expressive, while engine truth remains structured.

### Derivation guidance for specific public states

When lifecycle_phase = runnable:
- eligibility_state = eligible_now -> public_state = waiting
- eligibility_state = not_eligible_until_time -> public_state = delayed
- eligibility_state = blocked_by_dependencies AND blocking_reason = waiting_for_children -> public_state = waiting_children
- eligibility_state = blocked_by_budget OR blocked_by_quota -> public_state = rate_limited
- eligibility_state = blocked_by_route AND blocking_reason = waiting_for_capable_worker -> public_state = waiting
  with blocking_reason carrying the placement-specific explanation
- eligibility_state = blocked_by_lane_state -> public_state = waiting
  unless a later RFC introduces a distinct paused-lane rendering

This preserves a compact public taxonomy while still exposing precise root cause through the structured state fields.

### 2.10 Flow state model

Flow should use the same philosophy: structured internals, derived public summary.

Suggested flow view fields:
- topology_kind
- unresolved_dependency_count
- member_counts by public_state
- aggregate_usage
- failure_policy
- completion_condition_status
- derived_flow_status

Possible derived flow statuses:
- open
- running
- blocked
- completed
- failed
- cancelled

By default, a DAG flow may still derive `failed` even if some unrelated branches completed successfully, once required-success dependency failures caused one or more nodes to become terminal `skipped`.

These should be derived from member execution state plus flow policy.

### 2.11 Lane state model

Lane should remain simpler and not copy execution complexity.

Suggested lane states:
- intake_open
- intake_paused
- draining
- disabled

Lane state affects submission and claim behavior, not execution truth.

## 2.12 Resolved policy model

The engine must distinguish between policy **sources** and the **resolved policy** actually governing an execution.

### Policy sources
Possible policy sources include:
- engine defaults
- lane defaults
- flow defaults
- submission-time overrides
- retry-time adjustments
- fallback-time adjustments
- operator overrides

### ResolvedExecutionPolicy

Decision: use two persisted policy snapshots plus live runtime context.

FlowFabric should maintain:
- **ExecutionPolicySnapshot** — created when the execution is created
- **AttemptPolicySnapshot** — created when each attempt is created
- **RuntimeAdmissionContext** — computed live from current lane, route, quota/rate-limit, budget, and worker availability context

#### ExecutionPolicySnapshot
This captures what rules govern the logical execution over time.

Suggested contents:
- retry policy
- timeout policy template
- delay / schedule policy
- routing requirements and preferences
- budget attachments
- fallback policy template
- stream policy
- suspension defaults if any
- retention hints if any
- inherited lane/flow defaults after resolution

#### AttemptPolicySnapshot
This captures what policy the concrete attempt actually ran with.

Suggested contents:
- attempt timeout override if any
- route snapshot
- chosen provider/model snapshot
- actual fallback index used by this attempt if applicable
- attempt-specific pricing assumptions if any
- attempt-specific admission decisions that should be auditable

#### RuntimeAdmissionContext
This is live context, not policy.
It should be recomputed, not frozen into the execution snapshot.

Examples:
- current quota/rate-limit window status
- current remaining budget
- worker availability
- current route candidates
- lane paused/draining state
- current fairness/admission conditions

### Resolution principles

#### Principle P1 — nearest explicit override wins
Submission-time explicit execution policy should beat lane defaults.

#### Principle P2 — flow policy may supply defaults, not silently override explicit child policy
Flow should be able to contribute defaults or inherited attachments, but not invisibly replace explicit execution policy unless clearly requested.

#### Principle P3 — retry/reclaim/replay may recompute some policy, not all policy
New attempts should create a fresh AttemptPolicySnapshot.
That snapshot may reuse some fields from the execution snapshot while re-resolving route and admission context as needed.

Fallback chain/template belongs to execution-level policy lineage, while each attempt records the concrete fallback/provider/model it actually used.

#### Principle P4 — operator overrides must be auditable
Any privileged policy mutation after creation must be recorded.

### Open design questions

1. Which fields must always live on the ExecutionPolicySnapshot versus the AttemptPolicySnapshot?
2. Which fields should be frozen at execution creation versus re-resolved on every new attempt?
3. How much policy inheritance should dynamic child spawning get automatically from parent and flow?

## 2.13 Event and audit model

FlowFabric does not need full event sourcing to have a serious audit story.
But it does need a durable engine event model.

### Purpose
The event/audit model exists to answer:
- what happened
- in what order
- who caused it
- why this execution is in its current condition

### Event classes

#### Lifecycle events
- execution_created
- execution_became_runnable
- execution_claimed
- execution_completed
- execution_failed
- execution_cancelled
- execution_expired

#### Ownership events
- lease_acquired
- lease_renewed
- lease_expired
- lease_revoked
- execution_reclaimed

#### Control events
- execution_suspended
- signal_received
- execution_resumed
- execution_rerouted
- operator_override_applied
- control_operation_applied

#### Flow events
- flow_created
- execution_added_to_flow
- dependency_resolved
- flow_cancelled

#### Budget and policy events
- usage_reported
- budget_threshold_reached
- budget_exceeded
- fallback_advanced

### Event record fields
- event_id
- event_type
- target_object_type
- target_object_id
- related_execution_id if applicable
- related_flow_id if applicable
- actor_type
- actor_identity
- timestamp
- structured payload
- causation_id / correlation_id if available

### Event guarantees

The engine should treat core control/lifecycle events as durable append records.
Summaries may be projection-based, but these events are the minimum audit trail.

### Relationship to stream

Execution output stream is not the audit log.
Engine events are system facts.
Stream frames are execution-produced content.

## 3. Operation semantics

This section defines what kinds of correctness guarantees the engine should try to provide.

It does not require one exact backend implementation, but it does define what the system must mean.

### 3.1 Operation classes

#### Class A — Must-be-atomic state transitions
These operations should be atomic from the engine’s point of view:
- claim + lease acquisition
- completion with lease validation
- failure with retry scheduling decision
- suspend with lease release and suspension record creation
- signal acceptance with signal persistence and resume-condition evaluation
- budget usage increment with breach check, where strong enforcement is claimed
- dependency resolution that makes downstream execution eligible

#### Class B — Durable append operations
These must durably record information, but need not be grouped with major lifecycle transitions unless explicitly stated:
- append stream frame
- append log/progress
- append signal record
- append audit event

#### Class C — Derived / best-effort operations
These may be eventually consistent or projection-driven:
- aggregate counters
- metrics summaries
- list views
- route explanations
- flow summaries

The spec should be honest about which class each API belongs to.

### 3.2 Idempotency expectations

The engine should support idempotency in several places:

#### Submission idempotency
Prevent duplicate logical execution creation when callers retry submission.

#### Signal idempotency
Prevent duplicate callback delivery from producing repeated logical signals.

#### Completion / failure idempotency
A worker retrying the same completion call should not corrupt state if the lease and outcome already match.

### 3.3 Consistency expectations

The engine should aim for:
- strong correctness on claim/lease/complete/fail/suspend/resume paths
- stable per-execution stream ordering
- explicit, durable control actions
- eventually consistent projections for summaries and dashboards where needed

### 3.4 Replay and retry semantics

This needs explicit policy.

#### Retry
Retry is another **attempt** of the same logical execution.
It keeps execution identity and increments attempt index.

#### Reclaim
Reclaim after lease expiry also creates a new **attempt** of the same logical execution.
It differs from retry in cause:
- retry follows an explicit failed or retryable outcome
- reclaim follows interrupted ownership / liveness failure

Both preserve execution identity while creating a new attempt record.

#### Replay
Replay is an explicit operator or system action that re-executes prior work.
Replay creates a **new attempt on the same logical execution**.

Suggested replay fields:
- replay_count
- replay_reason
- replay_requested_by
- replayed_from_attempt_index

This keeps full execution history under one stable execution identity while still making replay explicit at the attempt layer.

### 3.5 Privileged override semantics

Certain operations may bypass normal lease/state restrictions if invoked by privileged operator context.
Examples:
- force cancel
- force fail
- force resume
- force reroute
- force revoke lease

These actions must always be auditable and clearly marked as overrides.

### 3.6 Retention and audit semantics

The engine should define retention classes for:
- execution core record
- stream frames
- signal history
- lease history
- flow membership / dependency history
- audit events

V1 can choose pragmatic defaults, but the categories should be clear in the spec.

---

## 4. V1 scope

This section draws a hard line between what FlowFabric v1 must do, what it should leave room for, and what it should explicitly defer.

### 4.1 V1 must-have

These are core to the product identity and should define the first serious version.

#### Execution core
- durable execution object
- decomposed execution state model
- retry attempts on same logical execution
- reclaim creates a new attempt on the same execution
- replay creates a new attempt on the same execution
- explicit cancel / fail / complete operations

#### Lease correctness
- single-owner lease model
- monotonic fencing / stale-owner rejection
- lease renewal
- reclaim after expiry

#### Suspension and signaling
- suspend from active execution
- durable signal delivery
- resume on satisfied signal condition
- suspension timeout policy with a minimal supported set

#### Stream
- append-only per-attempt stream
- stable ordering per attempt
- optional merged execution stream view
- read from offset and tail live
- separation from lifecycle event feed

#### Flow coordination
- real flow container object with optional root execution
- tree / parent-child flows
- fan-out / fan-in
- DAG support
- dynamic child spawning
- waiting-children behavior
- simple chain / stage pipeline
- dependency-local failure propagation by default

#### V1 flow constraints
- flow is a separate coordination container
- tree, chain, fan-out/fan-in, and DAG are in scope for v1
- dynamic expansion allowed
- basic DAG dependency semantics are in scope for v1
- rich multi-edge dependency semantics are deferred

#### AI-native resource features
- token usage reporting
- thinking-token usage reporting if supported by execution class
- cost usage reporting
- flow-scoped budget attachment
- separate quota/rate-limit policy surface
- fallback index / progression metadata

#### Capability and routing
- required capability matching
- worker registration + capability snapshot
- simple preferred lane/pool or locality hint if needed
- explainable unroutable / waiting-for-capable-worker / no-capacity state

#### Queue/lane facade
- named submission lane
- enqueue / delay / priority / retry defaults
- queue-compatible worker API over execution

#### Operability
- inspect execution state and blocking reason
- inspect lease and current owner
- inspect flow summary
- inspect budget usage
- operator control API for revoke / cancel / resume

### 4.2 V1 should-have if it does not distort the core

- DAG-lite beyond simple trees
- request/reply helper over execution waiters
- lane pause / drain behavior
- deduplicated submission
- bulk submission
- structured progress frames
- per-tenant visibility scoping

### 4.3 Designed-for but not required in V1

These should influence interfaces, but do not need to ship immediately.

- richer DAG edge semantics beyond v1 basics
- any-of / quorum / threshold dependency conditions
- cooperative lease handoff
- multi-condition ordered signal matching
- soft-limit budgets with escalation workflows
- route scoring and weighted preference matching
- durable artifact store integration beyond references
- advanced compensation / rollback choreography
- recurring scheduling as a rich top-level subsystem
- multi-backend execution support
- full local SQLite backend
- fairness-heavy global scheduler features beyond basic admission rules
- broad unification of budget and quota/rate-limit into one public primitive

### 4.4 Scheduling and admission semantics

FlowFabric needs an explicit scheduling/admission layer even if it is not a first-class user object.

### Purpose
Scheduling/admission decides which eligible execution may actually be claimed next.

### Inputs
- public state and eligibility condition
- priority
- lane state
- capability matching
- route availability
- budget signals
- quota/rate-limit signals
- tenant/fairness policy
- retry/backoff timing

### Minimal V1 behavior
V1 should support:
- eligibility filtering
- priority-aware selection within a lane
- basic cross-lane / cross-tenant fairness
- no-claim when requirements/capacity are not met
- explainable blocked reason when admission fails

V1 should **not** require a heavyweight global fairness scheduler to be considered complete.
But fairness is already a v1 concern and should not be treated as purely lane-local.

### Open design questions
1. Should priority ordering be lane-local only in v1?
2. Decision: fairness across tenants / lanes is a v1 concern, but with basic fairness guarantees rather than a heavyweight global scheduler.
3. Should rate/quota rejection happen before route resolution or after a candidate route exists?

### 4.5 Explicitly out of scope for V1

These should not be allowed to hijack the engine effort.

- full control-plane product
- memory / retrieval / vector database layer
- prompt registry
- eval product UI
- chat/session product model
- business approval domain model beyond suspension + signal primitives
- general policy engine for arbitrary org/business logic
- visual workflow builder
- “supports every backend equally” abstraction

### 4.6 V1 backend stance

FlowFabric v1 should be:
- **Valkey-native**
- designed with backend boundaries in mind
- not marketed as multi-backend yet

This should be explicit.

### 4.7 V1 wedge statement

The clearest current wedge is:

**FlowFabric v1 is a Valkey-native execution engine for long-running, interruptible, resource-aware AI and workflow jobs. It provides queue-compatible submission, durable suspend/resume via signals, flow coordination, partial-output streaming, lease-based correctness, and explainable routing.**

---

## 5. RFC extraction candidates

This working spec is rich enough that it should be split into focused RFCs later.

Recommended early RFCs:

1. Execution object and decomposed state model
2. Lease and fencing semantics
3. Suspension and signal semantics
4. Stream model and durability modes
5. Flow object and dependency semantics
6. Budget model and enforcement points
7. Capability and route selection model
8. Lane / queue compatibility facade
9. Valkey-native backend architecture and atomicity classes
10. V1 scope and explicit non-goals


---

# Finalization pass — locked decisions and authoritative clarifications

This section is the authoritative decision log for ambiguous areas discussed during spec shaping.
If any earlier wording is softer or inconsistent, this section wins.

## 1. Decision-making rule

Obvious decisions should not be reopened just to ask a question.
When correctness, crash healing, maintainability, or UX clearly point one way, the spec should simply choose.
Questions should be reserved for real semantic forks.

## 2. Authoritative state model stance

FlowFabric uses a **constrained orthogonal state vector**.

The authoritative dimensions are:
- lifecycle phase
- ownership state
- eligibility state
- blocking reason
- terminal outcome
- attempt state

The engine also exposes a stable **public_state** derived centrally by the engine.
Clients must not invent their own derivation rules.

Chosen public states:
- waiting
- delayed
- rate_limited
- waiting_children
- active
- suspended
- completed
- failed
- cancelled
- expired
- skipped

`skipped` is required for DAG correctness when a node becomes impossible to run because a required dependency failed.

## 3. Attempt is real

Attempt is a first-class inspectable object.
Execution remains the primary API surface.

Authoritative attempt rules:
- retry creates a new attempt on the same execution
- reclaim after lease expiry creates a new attempt on the same execution
- replay after terminal completion/failure creates a new attempt on the same execution
- lease renewal does not create a new attempt
- signal delivery does not create a new attempt

Execution identity is stable across retries, reclaims, and replays.
Attempt history is the concrete run timeline.

## 4. Replay semantics

Replay does **not** create a new execution.
Replay reuses the same logical execution identity and creates a new replay attempt.

Replay metadata should be preserved on the execution history, including fields such as:
- replay_count
- replay_reason
- replay_requested_by
- replayed_from_attempt_index

This keeps the full history of one logical execution under one object.

## 5. Stream semantics

The true stream is **attempt-scoped**.

That means:
- each attempt owns its own ordered output stream
- replay/retry/reclaim output is cleanly attributable to the attempt that produced it
- execution may expose a **merged execution stream view** for UX convenience

The merged execution view is a convenience surface, not the deepest source of truth.

## 6. Policy model

FlowFabric uses three policy/context layers:

### ExecutionPolicySnapshot
Created when the execution is created.
Represents the resolved logical policy for the execution.

### AttemptPolicySnapshot
Created when each attempt is created.
Represents the concrete policy the attempt actually ran with.

### RuntimeAdmissionContext
Computed live.
Represents current dynamic conditions such as:
- worker availability
- route availability
- quota/rate-limit windows
- current remaining budget
- lane paused/draining state
- current fairness/admission conditions

Fallback chain/template belongs to execution-level policy lineage.
Each attempt records the concrete fallback/provider/model it actually used.

## 7. Waitpoint model

The term `token` should not be used here because it is overloaded in AI contexts.

Authoritative naming:
- `waitpoint_id` = internal durable identity of a waiting episode
- `waitpoint_key` = external opaque handle for callback/approval delivery

A waitpoint is the specific correlation point for one waiting episode.

### Waitpoint rules
- resume/data signals are scoped to executions and waitpoints
- passive flow containers do not own waitpoints
- a waitpoint may accept **multiple signals until it closes**
- signal idempotency still applies
- once a waitpoint is satisfied/closed, later signals are rejected or treated as no-op according to policy
- one-shot approval UX may still be layered on top of a multi-signal waitpoint model

### Pending waitpoints
To solve callback/approval races cleanly, the engine may support **pending waitpoints**.

That means:
- a waitpoint may be precreated before full suspension commit
- early signals may be accepted against that waitpoint
- when suspension commits, the waitpoint becomes attached to that suspension record
- if the execution never actually enters that waitpoint, it must expire or close clearly

This is a correctness feature, not generic signal buffering.

## 8. Signal versus control boundary

For maintainability, active control should not be modeled as generic signals.

### Signals are for
- approval result
- callback payload
- tool result
- continue/resume input tied to a waitpoint

### Explicit control operations are for
- cancel_execution
- revoke_lease
- reroute_execution
- pause_lane
- operator_override

This keeps the signal primitive narrow and prevents it from becoming a generic mutation chute.

## 9. Flow model

Flow is a real **coordination container** with optional root execution.
A root execution is common and useful, but it is not mandatory for a flow to exist.

### Important boundary
The flow container itself remains **passive**.
If orchestration needs to wait/suspend/resume, that behavior lives on a real coordinator execution inside the flow, not on the passive flow container itself.

This keeps one waiting model, one lease/reclaim model, and one replay model.

## 10. DAG in v1

DAG support is **in scope for v1**.

### Default dependency semantics
The default and most important dependency kind is **success-only**.
A downstream node becomes eligible only when its required predecessors completed successfully.

### Default failure propagation
The default is **dependency-local propagation**, not global fail-fast.

That means:
- a failed node affects only nodes that depend on it
- unrelated branches may continue
- downstream nodes whose required dependencies can no longer be satisfied become terminal `skipped`
- the overall flow may still derive `failed` depending on aggregate policy

### Deferred to later versions
The following richer DAG semantics are explicitly v2 candidates:
- any-of dependencies
- quorum dependencies
- threshold / partial-join dependency conditions
- richer multi-edge semantics beyond v1 basics

## 11. Resource-control model

Budget, quota, and rate-limit are **separate user-facing policy families**.
They may share enforcement machinery internally, but they must remain distinct in the public model.

### Budget
Budget is about **consumable allowance**.
Examples:
- cost
- total tokens
- input/output tokens
- thinking tokens
- future extensible AI/runtime resource dimensions

Budget dimensions are **extensible**, not a closed enum.

### Quota / rate-limit
Quota/rate-limit are about **admission and pacing**.
Examples:
- requests per window
- tokens per minute
- active concurrency caps
- tenant throttling

Different policy families may trigger different behaviors.
Examples:
- budget breach may stop/close/suspend/degrade
- quota breach may defer to later window
- rate-limit breach may jitter/backoff/throttle

## 12. Fairness is in v1

Cross-lane and cross-tenant fairness are **v1 concerns**.

V1 does not require a heavyweight global scheduler, but it does require:
- more than purely lane-local priority
- basic fairness across lanes/tenants
- explainable admission behavior

## 13. Idempotency and dedup are first-class

FlowFabric v1 should provide a first-class idempotency/dedup contract.
This is not optional sugar.

At minimum, the engine should support:
- submission idempotency
- signal idempotency
- replay/retry-safe mutation behavior where possible
- worker-facing hooks/contract surface for side-effect-safe execution patterns

This is necessary for crash recovery, reclaim, replay, and external callback correctness.

## 14. RFC slicing recommendation

The current spec is now rich enough to slice into RFCs.
The recommended order is:

1. Execution object + orthogonal state vector
2. Attempt model + replay/reclaim/retry lineage
3. Lease + fencing semantics
4. Suspension + waitpoint model
5. Signal model and control boundary
6. Stream model (attempt-scoped + merged execution view)
7. Flow container + DAG semantics
8. Budget / quota / rate-limit policy families
9. Scheduling, fairness, and admission
10. Valkey-native architecture mapping

## 15. Remaining acceptable ambiguity

The remaining open questions should be treated as targeted RFC work, not blockers for the core spec.
Examples:
- exact validity matrix for all state-vector combinations
- exact retention defaults
- exact built-in resource dimension set for default UX
- exact fairness algorithm in v1
- exact route scoring rules beyond v1 basics

