# Learning Guide: Hatchet - Distributed Task Queue and Workflow Orchestrator

**Generated**: 2026-04-09
**Sources**: 42 resources analyzed
**Depth**: deep

## Prerequisites

- Familiarity with at least one of: Python (3.10+), TypeScript/Node.js, or Go
- Basic understanding of distributed systems concepts (queues, workers, retries)
- Docker installed for local development / self-hosting
- PostgreSQL familiarity helpful but not required

## TL;DR

- Hatchet is an open-source (MIT) durable task queue and workflow orchestrator built on PostgreSQL, supporting Python, TypeScript, Go, and Ruby (early access) SDKs.
- Tasks are plain functions; workflows compose tasks into DAGs or dynamic durable graphs with automatic retry, timeout, concurrency control, and rate limiting.
- The architecture separates a centralized control plane (engine + API + dashboard + Postgres) from distributed workers that run on your infrastructure.
- Durable execution checkpoints at `SleepFor`, `WaitForEvent`, `RunChild`, and `Memo` calls, evicting tasks from worker slots during waits and resuming from the exact checkpoint after interruptions.
- Available as Hatchet Cloud (managed) or fully self-hosted via Docker Compose, Hatchet Lite, or Kubernetes Helm charts.

---

## Core Concepts

### Tasks

The fundamental unit of work in Hatchet is a **task** -- a function that receives typed input and a context object, and returns a JSON-serializable output. Every task invocation is durably persisted.

**Python:**
```python
@hatchet.task()
def my_task(input: MyInput, ctx: Context) -> MyOutput:
    return MyOutput(result=input.value * 2)
```

**TypeScript:**
```typescript
const myTask = hatchet.task({
  name: "my-task",
  fn: async (input, ctx) => {
    return { result: input.value * 2 };
  },
});
```

**Go (v1 Reflection SDK):**
```go
task := client.NewStandaloneTask("my-task",
  func(ctx hatchet.Context, input MyInput) (*MyOutput, error) {
    return &MyOutput{Result: input.Value * 2}, nil
  },
  hatchet.WithRetries(3),
)
```

Tasks can be configured with: retry policies, execution/schedule timeouts, concurrency limits, rate limits, priority levels, and worker label affinity.

### Workers

Workers are long-running processes that register tasks with Hatchet and receive assignments from the queue. Key properties:

- **Slots**: Controls concurrent task capacity per worker (default: 100). If set to 5, the worker runs at most 5 tasks concurrently.
- **Durable Slots**: Separate slot pool for durable tasks (which may sleep/wait and be evicted).
- **Labels**: Key-value pairs for affinity-based routing.
- **Lifespans**: Async generator pattern (like FastAPI) for startup/shutdown hooks, sharing immutable state (e.g., DB connection pools, loaded ML models) across all tasks on the worker. Experimental feature.

**Starting a worker:**
```bash
# CLI (recommended, supports hot reload)
hatchet worker dev

# Or programmatically with HATCHET_CLIENT_TOKEN env var
python worker.py
```

Multiple workers can register the same tasks for horizontal scaling. Hatchet distributes work across them automatically.

### Workflows (DAG Tasks)

Workflows compose multiple tasks into a directed acyclic graph (DAG) with explicit step dependencies. Each step can access parent step outputs. Hatchet automatically routes outputs between parent and child steps.

### Durable Tasks

Durable tasks enable workflows where the execution graph emerges at runtime. They checkpoint at specific operations:

| Operation | Purpose |
|-----------|---------|
| `SleepFor` | Pause for a duration; worker slot freed; server-side time tracking |
| `WaitForEvent` | Pause until external event arrives (human-in-the-loop) |
| `RunChild` | Spawn child task and await result |
| `Memo` | Cache function output based on inputs |

When interrupted (crash, restart, scale-down), durable tasks resume from the last checkpoint on any available worker -- no re-execution of completed work. If a 24-hour sleep is interrupted after 23 hours, only 1 hour remains.

---

## Triggers

### Programmatic API (Run / Run-and-Wait / Fire-and-Forget)

Tasks can be triggered from application code using the SDK client. Modes include:
- **Run and wait**: Trigger and block until result is available
- **Fire and forget**: Trigger without waiting for completion
- **Bulk trigger**: Trigger many task instances in a single API call for high throughput

### Event-Driven Triggers

Push events to Hatchet and have tasks react:

```python
# Push an event
hatchet.event.push("user:create", {"user_id": "1234", "should_skip": False})

# Durable task waits for event with CEL filter
res = await ctx.aio_wait_for_event("user:update", "input.user_id == '1234'")
```

DAG tasks support three event-based operators:
- **wait_for**: Pause until event arrives
- **skip_if**: Skip task if event arrives beforehand
- **cancel_if**: Cancel task (and downstream) upon event arrival

All event conditions support CEL (Common Expression Language) filtering.

### Cron Schedules

```python
cron_workflow = hatchet.workflow(name="CronWorkflow", on_crons=["0 0 * * *"])

@cron_workflow.task()
def daily_job(input: EmptyModel, ctx: Context) -> dict:
    return {"status": "done"}
```

Supports 5-field and 6-field cron expressions. Schedules operate in UTC. Cron triggers can also be created programmatically or through the dashboard. Missed runs during downtime are not automatically rescheduled.

### One-Time Scheduled Runs

Schedule tasks for a specific future time. Supports listing, deletion, rescheduling, and bulk operations.

### Webhook Triggers

External systems trigger workflows via HTTP. Supports pre-configured sources (Stripe, GitHub, Slack) or generic webhooks with HMAC/API-key/basic-auth validation. Event key expressions use CEL to extract routing information from payloads.

---

## Concurrency Control

Three strategies available at both task and workflow levels:

| Strategy | Behavior |
|----------|----------|
| `GROUP_ROUND_ROBIN` | Distribute slots fairly across concurrency groups in round-robin fashion |
| `CANCEL_IN_PROGRESS` | Cancel running instances for the same key when capacity exhausted |
| `CANCEL_NEWEST` | Cancel newly queued runs, let existing work finish |

Configuration uses a CEL expression for grouping and a `max_runs` cap:

```python
@hatchet.task(
    concurrency=ConcurrencyExpression(
        expression="input.user_id",
        max_runs=1,
        limit_strategy=ConcurrencyLimitStrategy.GROUP_ROUND_ROBIN,
    )
)
```

Use cases: per-user fairness, resource-intensive operations, race condition prevention, traffic spike management.

---

## Rate Limiting

### Static Rate Limits

Defined at worker startup for known resources:

```python
# Declare the limit
hatchet.admin.put_rate_limit("api-calls", limit=10, duration=RateLimitDuration.MINUTE)

# Apply to task
@hatchet.task(rate_limits=[RateLimit(key="api-calls", units=1)])
```

### Dynamic Rate Limits

Use CEL expressions evaluated at runtime for per-user/per-tenant limits:

```python
@hatchet.task(
    rate_limits=[
        RateLimit(
            dynamic_key="input.user_id",
            units=1,
            limit=10,
            duration=RateLimitDuration.MINUTE,
        )
    ]
)
```

Tasks exceeding limits are re-queued, not dropped. Best practice: apply rate limits to entry steps to gate entire downstream workflows.

---

## Priority Queues

Tasks support integer priority levels. Higher-priority tasks are dequeued before lower-priority ones. Priority can be set at workflow definition time or per-invocation:

```go
client.RunWorkflow("my-task", input, WithPriority(10))
```

---

## Reliability Features

### Retries

Configure retry count and backoff per task. Tasks that time out also trigger retries if configured.

```python
@hatchet.task(retries=5)  # With optional backoff configuration
```

### Timeouts

Two timeout types:

| Type | Default | Purpose |
|------|---------|---------|
| **Schedule timeout** | 5 min | Max time a task waits in queue before cancellation |
| **Execution timeout** | 60 sec | Max time for task completion after starting |

Format: `"10s"`, `"4m"`, `"1h"`. Python also accepts `datetime.timedelta`.

**Timeout refresh** (additive):
```python
ctx.refresh_timeout(timedelta(seconds=10))  # Adds 10s to current timeout
```

### Cancellation

Multiple mechanisms for graceful termination:
- **Exit flag polling**: `ctx.exit_flag`
- **Self-cancellation**: `ctx.aio_cancel()`
- **Abort signals**: Standard `AbortSignal` in TypeScript, `ctx.Done()` in Go
- **Bulk cancellation**: Cancel multiple tasks by IDs or filter criteria

### On-Failure Tasks

A special task type that automatically runs when any workflow task fails. One per workflow:

```python
@my_workflow.on_failure_task()
def on_failure(input: EmptyModel, ctx: Context) -> dict:
    print(ctx.task_run_errors)
    return {"status": "cleanup_done"}
```

---

## SDK Reference

### Python SDK

- **Package**: `hatchet-sdk` (PyPI, v1.32.1 as of April 2026)
- **Requirements**: Python 3.10+
- **Install**: `pip install hatchet-sdk`
- **Optional extras**: `[otel]` for OpenTelemetry, `[claude]` / `[openai]` for AI integrations
- **Key features**: Pydantic input/output validation, FastAPI-style dependency injection (experimental), async/await native, `asyncio.to_thread()` for blocking code, lifespan hooks
- **Design philosophy**: Inspired by FastAPI, type-safe, decorator-based task definitions

### TypeScript SDK

- **Package**: `@hatchet-dev/typescript-sdk` (npm)
- **Key features**: Full TypeScript type safety, AbortSignal for cancellation, streaming support
- **Warning**: Avoid blocking the event loop; use `setTimeout` or worker threads for CPU-intensive work

### Go SDK

- **Package**: `github.com/hatchet-dev/hatchet/sdks/go` (v1 Reflection SDK, current)
- **Note**: The legacy `pkg/client` (v0) and `pkg/v1` (v1 Generics) packages are **deprecated**
- **Key features**: Typed input/output structs with JSON tags, functional options pattern, context-based cancellation
- **Migration**: See official migration guide for v0-to-v1 changes

### Ruby SDK (Early Access)

- Uses `HATCHET.task()` blocks
- Feature set still maturing

### Cross-SDK Features

All SDKs provide consistent access to:
- Client setup with `HATCHET_CLIENT_TOKEN` env var
- Workflow/task/worker definitions
- Cron, schedule, event clients
- Run management (trigger, cancel, get details)
- Streaming (put/subscribe to real-time task output)
- OpenTelemetry instrumentation
- Additional metadata for filtering/searching

---

## Streaming

Tasks can stream data back to consumers in real-time:

```python
# Producer (inside task)
await ctx.aio_put_stream(chunk)

# Consumer
async for chunk in hatchet.runs.subscribe_to_stream(ref.workflow_run_id):
    print(chunk, flush=True, end="")
```

Consumers must subscribe before events publish; pre-published events are dropped. For web apps, use your backend as a proxy (e.g., FastAPI `StreamingResponse`).

---

## Observability

### Dashboard

Real-time web UI for monitoring all workflows, task runs, and worker health. Features:
- Filtering by metadata, status, time range
- Drill-down into individual runs and steps
- Replay failed runs directly from the UI
- Worker health and slot utilization
- Failure alerts (Slack, email configurable)

### OpenTelemetry Integration

SDKs generate producer spans (client-side) and consumer spans (worker-side) with rich metadata. W3C `traceparent` headers automatically establish parent-child trace relationships.

```python
from hatchet_sdk.opentelemetry import HatchetInstrumentor
HatchetInstrumentor().instrument()
```

### Logging

Tasks can emit structured logs via context, visible in the dashboard.

### Additional Metadata

Attach arbitrary key-value string pairs to runs, events, and cron triggers for filtering and organization. Metadata propagates from parent to child.

---

## Self-Hosting

### Architecture Components

| Component | Purpose |
|-----------|---------|
| **Engine** | Core orchestration, gRPC task scheduling |
| **REST API** | Workflow administration |
| **Dashboard** | Web UI (default: port 8080) |
| **PostgreSQL** | State, metadata, workflow persistence |
| **RabbitMQ** (optional) | Inter-service communication, real-time updates |

### Deployment Options

**Hatchet Lite** (dev/low-volume):
- Single Docker image: `ghcr.io/hatchet-dev/hatchet/hatchet-lite:latest`
- CLI: `hatchet server start`
- Default dashboard at `localhost:8888`

**Docker Compose** (production):
- PostgreSQL 15.6, RabbitMQ 3, migration service, engine, dashboard
- Default ports: 8080 (dashboard), 7077 (gRPC), 5435 (Postgres), 5673 (RabbitMQ)
- Can replace RabbitMQ with Postgres-only: `SERVER_MSGQUEUE_KIND=postgres`

**Kubernetes** (production HA):
- Helm chart deployment
- Separate deployments for gRPC, controllers, schedulers with configurable replicas
- Reference Terraform examples for GCP HA setup

### High Availability

- **Database**: Use managed Postgres (AWS RDS, Google Cloud SQL) with HA
- **Message Queue**: RabbitMQ cluster with 3+ replicas across availability zones
- **Engine**: Multiple replicas behind load balancer (e.g., 4 gRPC + 2 controller + 2 scheduler)
- **Read replicas**: Configure `READ_REPLICA_ENABLED=true` with separate connection string for read-heavy UI/analytics queries

### Key Configuration

| Variable | Purpose | Default |
|----------|---------|---------|
| `HATCHET_CLIENT_TOKEN` | Worker authentication | -- |
| `HATCHET_CLIENT_TLS_STRATEGY` | TLS mode (`none` for self-hosted without TLS) | -- |
| `SERVER_MSGQUEUE_KIND` | `rabbitmq` or `postgres` | rabbitmq |
| `SERVER_GRPC_BROADCAST_ADDRESS` | Engine gRPC address for workers | -- |
| `SERVER_GRPC_WORKER_STREAM_MAX_BACKLOG_SIZE` | Max pending messages per worker stream | 20 |
| `SERVER_LIMITS_DEFAULT_TENANT_RETENTION_PERIOD` | Data retention (Go duration) | 720h (30 days) |
| `READ_REPLICA_ENABLED` | Enable Postgres read replica | false |
| `READ_REPLICA_DATABASE_URL` | Read replica connection string | -- |

### Prometheus Metrics

Hatchet exposes Prometheus-compatible metrics for monitoring engine performance. Integration documented in self-hosting guides.

---

## Hatchet Cloud

Managed offering at `cloud.onhatchet.run`:

| Tier | Price | Includes |
|------|-------|----------|
| **Developer** (free) | $10/1M runs | First 100K runs free, SOC 2 Type II |
| **Team** | $500/mo + usage | 10 users, 5 tenants, 3-day retention, 500 RPS |
| **Scale** | $1,000/mo + usage | Unlimited users/tenants, 7-day retention, HIPAA, audit logs |
| **Enterprise** | Custom | 300M+ runs/mo, latency guarantees, SSO/SAML, BYOC, custom SLAs |

Annual plans offer 20% savings.

---

## Comparison with Temporal

| Aspect | Hatchet | Temporal |
|--------|---------|---------|
| **Data store** | PostgreSQL (single dependency) | Cassandra/MySQL/Postgres + Elasticsearch |
| **Complexity** | Simpler -- tasks are plain functions with decorators | More complex -- requires understanding of workflows, activities, signals, queries as separate concepts |
| **DSL/Language** | No DSL; native language SDKs | No DSL but imposes strict determinism constraints on workflow code |
| **Durable execution** | Checkpoint-based (SleepFor, WaitForEvent, RunChild, Memo) | Full event-sourced replay of entire workflow history |
| **Setup** | Postgres-only minimal deployment possible | Requires multiple services (frontend, history, matching, worker) |
| **Target audience** | Teams wanting task queue + workflow without heavy infra | Teams needing enterprise-grade workflow orchestration at massive scale |
| **Multi-tenancy** | Built-in tenant isolation with fair queueing | Namespace-based isolation |
| **Maturity** | Younger project (~2 years), rapidly evolving | Battle-tested at large scale (Uber, Netflix, etc.) |
| **Cloud offering** | Hatchet Cloud | Temporal Cloud |
| **License** | MIT | MIT (server core) |

Hatchet positions itself as a simpler alternative that combines task queue functionality (like Celery/BullMQ) with workflow orchestration (like Temporal) in a single PostgreSQL-backed platform.

---

## Comparison with Celery

| Aspect | Hatchet | Celery |
|--------|---------|--------|
| **Async support** | Native async/await | No asyncio support |
| **Rate limiting** | Global across all workers (static + dynamic) | Per-worker only |
| **Concurrency** | Per-task, per-key with strategies | No task-level concurrency controls |
| **Dead lettering** | Built-in | Requires broker-specific config |
| **Observability** | Persistent dashboard with filtering, replay | Flower (loses data on restart) |
| **Scheduling** | Native cron + one-time schedules | Requires Celery Beat |
| **ETA tasks** | Server-side persistence (no memory bloat) | In-worker memory |
| **Default timeouts** | Execution: 60s, Schedule: 5min | None (dangerous) |
| **Acknowledgement** | After completion (safe) | Early ack by default (loses tasks on crash) |

---

## Comparison with BullMQ

| Aspect | Hatchet | BullMQ |
|--------|---------|--------|
| **Backend** | PostgreSQL | Redis |
| **Language** | Python, TypeScript, Go, Ruby | TypeScript/JavaScript |
| **Durability** | PostgreSQL persistence (survives restarts) | Redis (data loss risk without persistence config) |
| **Workflow support** | DAG + durable execution | Flow (parent-child) |
| **Multi-tenancy** | Native fair queueing | Manual queue-per-tenant |
| **Dashboard** | Built-in | BullMQ Pro / Bull Board |

---

## Common Pitfalls

| Pitfall | Why It Happens | How to Avoid |
|---------|---------------|--------------|
| Blocking the event loop (Python/TS) | Synchronous calls in async tasks | Use `asyncio.to_thread()` or `setTimeout` |
| Oversized payloads | Passing large data as task input/output | Keep payloads under 4MB; use object storage for large data |
| Phantom workers in dashboard | Leaked processes or long termination | Proper SIGTERM handling; kill stray processes |
| Tasks stuck in QUEUED | No worker registered for task, or slots full | Verify worker registrations and slot availability |
| Missed cron runs after downtime | Hatchet does not backfill missed schedules | Design idempotent catch-up logic if needed |
| Timeout refresh misunderstanding | `refresh_timeout` is additive, not a reset | Calculate remaining time before refreshing |
| Mutable state in lifespans | Race conditions across concurrent tasks | Store only immutable data in lifespan state |
| Worker disconnection loops | Resource exhaustion or network instability | Monitor CPU/memory; deploy workers near engine |
| SDK version mismatch | Old SDK with new engine or vice versa | Keep SDK versions aligned with engine version |

---

## Best Practices

1. **Use Pydantic/typed models for task I/O** -- Catches schema errors before execution, enables IDE support (Source: Hatchet Python SDK docs)
2. **Apply rate limits to entry steps** -- Gates entire downstream workflows rather than individual mid-flow steps (Source: Hatchet rate-limits docs)
3. **Set explicit timeouts on all tasks** -- Prevents queue stalls from hung tasks; default 60s execution timeout is a safety net, not a target (Source: Hatchet timeouts docs)
4. **Use durable sleep instead of language sleep** -- Frees worker slots and survives restarts with server-side time tracking (Source: Hatchet durable-sleep docs)
5. **Deploy workers geographically near the engine** -- Network latency dramatically affects throughput (2ms latency dropped Postgres throughput 5x in benchmarks) (Source: Hatchet blog, fastest Postgres inserts)
6. **Use lifespans for expensive resource initialization** -- DB connection pools, ML models loaded once and shared across all tasks (Source: Hatchet lifespans docs)
7. **Implement on-failure tasks for cleanup** -- Declarative error handling for notifications, resource release, compensating actions (Source: Hatchet on-failure docs)
8. **Use additional metadata liberally** -- Environment tags, source identification, and business categorization improve dashboard discoverability (Source: Hatchet metadata docs)
9. **Batch inserts for high-throughput producers** -- Even 25-row batches nearly saturate throughput (Source: Hatchet blog, fastest Postgres inserts)
10. **Use GROUP_ROUND_ROBIN for multi-tenant fairness** -- Prevents any single tenant from monopolizing worker slots (Source: Hatchet concurrency docs)

---

## Architecture Deep Dive: PostgreSQL Optimizations

Hatchet's engine is built on PostgreSQL with significant performance work:

- **COPY-based inserts** achieve 62K+ writes/second (31x over naive sequential inserts)
- **In-memory write buffers** flush on time intervals or capacity thresholds, with backpressure via blocking writes
- **Connection pool tuning**: Diminishing returns beyond ~20 connections due to CPU saturation and lock contention
- **Read replicas** offload dashboard/analytics queries from the primary write path
- **Data retention** policies automatically prune completed runs older than configurable thresholds (default 30 days)

The team chose PostgreSQL over specialized queue backends because it serves as both the durable state store and message queue (with optional RabbitMQ for higher real-time throughput).

---

## Python SDK Examples Catalog

The official repository includes 50+ examples covering every feature:

| Category | Examples |
|----------|----------|
| **Basics** | simple, quickstart, dag, fanout, child |
| **Scheduling** | cron, scheduled, delayed, durable_sleep |
| **Concurrency** | concurrency_limit, concurrency_limit_rr, concurrency_cancel_in_progress, concurrency_cancel_newest, concurrency_multiple_keys, concurrency_workflow_level |
| **Rate Limiting** | rate_limit |
| **Routing** | affinity_workers, sticky_workers, runtime_affinity |
| **Resilience** | retries, non_retryable, on_failure, on_success, return_exceptions, timeout, cancellation |
| **Events** | events, durable_event, webhooks, webhook_with_scope |
| **Advanced** | conditions, priority, durable, durable_eviction, agent, bulk_fanout, bulk_operations, manual_slot_release, dependency_injection, streaming |
| **Integration** | fastapi_blog, opentelemetry_instrumentation, lifespans, unit_testing |

---

## Go SDK Migration Path

The Go SDK has gone through three versions:

1. **V0** (`pkg/client`): Original, deprecated
2. **V1 Generics** (`pkg/v1`): Type-safe with generics, deprecated
3. **V1 Reflection** (`sdks/go`): **Current standard** -- reflection-based, simpler API

Key migration change: `WorkflowJob` + `RegisterWorkflow()` replaced by `NewStandaloneTask()` with functional options. Function signatures now require explicit typed input/output structs.

---

## Further Reading

| Resource | Type | Why Recommended |
|----------|------|-----------------|
| [Hatchet Documentation](https://docs.hatchet.run) | Official Docs | Comprehensive reference with multi-language examples |
| [GitHub Repository](https://github.com/hatchet-dev/hatchet) | Source Code | 6.8K stars, MIT license, active development |
| [Python SDK Examples](https://github.com/hatchet-dev/hatchet/tree/main/sdks/python/examples) | Examples | 50+ examples covering every feature |
| [Problems with Celery](https://hatchet.run/blog/problems-with-celery) | Blog | Motivation for Hatchet, Celery limitations analysis |
| [Task Queue for Modern Python](https://hatchet.run/blog/task-queue-modern-python) | Blog | Python-specific features, FastAPI integration |
| [Fastest Postgres Inserts](https://hatchet.run/blog/fastest-postgres-inserts) | Blog | Performance engineering behind Hatchet's engine |
| [How to Think About Durable Execution](https://hatchet.run/blog/how-to-think-about-durable-execution) | Blog | Durable execution philosophy and design |
| [On SDK Design](https://hatchet.run/blog/on-sdk-design) | Blog | Multi-language SDK design decisions |
| [PyPI: hatchet-sdk](https://pypi.org/project/hatchet-sdk/) | Package | Python SDK v1.32.1, install instructions |
| [Go SDK (pkg.go.dev)](https://pkg.go.dev/github.com/hatchet-dev/hatchet/sdks/go) | Package | Go SDK reference (current v1 Reflection) |
| [Hatchet Cloud](https://cloud.onhatchet.run) | Platform | Managed offering, free tier available |
| [Self-Hosting: Docker Compose](https://docs.hatchet.run/self-hosting/docker-compose) | Guide | Production Docker deployment |
| [Self-Hosting: HA](https://docs.hatchet.run/self-hosting/high-availability) | Guide | Multi-replica, multi-zone deployment |
| [Reflecting on Two Years](https://hatchet.run/blog/reflecting-on-two-years) | Blog | Project history and open-source startup lessons |
| [Pitfalls of Partitioning Postgres](https://hatchet.run/blog/pitfalls-partitioning-postgres) | Blog | Advanced Postgres architecture decisions |

---

*This guide was synthesized from 42 sources. See `resources/hatchet-sources.json` for full source list with quality scores.*
