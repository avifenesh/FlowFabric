# Learning Guide: Temporal -- Durable Execution Platform for Microservices Orchestration

**Generated**: 2026-04-09
**Sources**: 42 resources analyzed
**Depth**: deep

---

## Prerequisites

- Familiarity with distributed systems concepts (eventual consistency, CAP theorem, failure modes)
- Experience building microservices or backend applications
- Understanding of message queues, event-driven architectures, and async processing
- Proficiency in at least one of: TypeScript, Go, Java, Python, or .NET
- Basic understanding of database transactions and the saga pattern

## TL;DR

- Temporal is an open-source **durable execution platform** that guarantees your application code runs to completion despite crashes, network failures, or infrastructure outages -- it replays event history to restore state exactly where it left off.
- You write **Workflows** (deterministic orchestration logic) that call **Activities** (non-deterministic side-effects like API calls, DB writes). Workers poll Task Queues and execute your code; the Temporal Service coordinates state.
- The platform supports **7 official SDKs** (Go, Java, Python, TypeScript, .NET, PHP, Ruby) and offers both **self-hosted** (backed by Cassandra, MySQL, or PostgreSQL) and **Temporal Cloud** (managed SaaS starting at $100/month).
- Key interaction patterns include **Signals** (async writes), **Queries** (sync reads), and **Updates** (sync writes with validation). **Nexus Operations** enable cross-namespace service orchestration.
- Temporal originated as a fork of Uber's Cadence, is written in Go, has 19.5k+ GitHub stars, and is used at scale by companies like Netflix, Replit, Vinted, and Snap.

---

## Core Concepts

### 1. Durable Execution

Temporal's defining feature is **durable execution**: the guarantee that your code runs to completion no matter what. If a process crashes or the backend service goes down, your program resumes in exactly the same state with all local variables and stack traces intact.

**How it works internally:**

1. The Temporal Service maintains an **Event History** -- an append-only log recording every state transition
2. When a Worker crashes, the system performs **deterministic replay**: it re-executes the Workflow code from the beginning, using the Event History to skip already-completed steps
3. Once replay catches up to the last recorded event, execution continues forward normally
4. All updates across databases, queues, and state stores are **atomic** -- Temporal uses Transfer Queues (transactional outbox pattern) to achieve consistency without two-phase commits

**Key insight**: Developers write standard code (not YAML/JSON DSLs) while Temporal automatically handles durability. Sleep calls can span days or years without tying up resources.

### 2. Workflows

A Workflow is a **deterministic function** that orchestrates your business logic. Workflows are resilient -- they can run for years, surviving infrastructure failures.

**Deterministic constraints** -- Workflow code must NOT:
- Use random number generators (use SDK-provided alternatives)
- Access system time directly (use `workflow.Now()` or SDK time utilities)
- Make network calls or I/O (delegate to Activities)
- Use mutable global state
- Use non-deterministic language features (threading, etc.)

**Workflow Execution lifecycle:**
- Each execution is identified by a **Workflow ID** (user-chosen, unique per namespace) and **Run ID** (system-generated UUID)
- Progress is recorded as **Commands** (requested actions) and **Events** (recorded facts)
- Executions have configurable timeouts: Execution Timeout, Run Timeout, and Task Timeout

**Event History limits:**
- Warning at 10,240 events or 10 MB
- Error at 51,200 events or 50 MB
- Use **Continue-As-New** to reset history for long-running workflows

### 3. Activities

An Activity is a **normal function** that executes a single, well-defined action -- calling another service, writing to a database, sending an email. Unlike Workflows, Activities can contain non-deterministic code.

**Activity timeout types:**

| Timeout | Purpose | Default | Recommendation |
|---------|---------|---------|----------------|
| Schedule-To-Start | Time waiting in task queue for a worker | Infinity | Set to detect worker fleet issues |
| Start-To-Close | Maximum single attempt execution time | Required | Always set for production |
| Schedule-To-Close | Overall limit across all retries | Infinity | Use to cap total duration |
| Heartbeat | Maximum silence between heartbeat pings | None | Set for long-running activities |

**Key patterns:**
- Activities should be **idempotent** where possible -- they may be retried
- **Heartbeating** lets long-running activities report progress and enables early failure detection
- **Local Activities** run in the same worker process as the workflow, avoiding a network round-trip (useful for short, fast operations)
- **Standalone Activities** can be invoked directly from a client without a Workflow
- If an Activity fails, the next retry starts from initial state unless you checkpoint via heartbeat details

### 4. Workers

Workers are **your application processes** that poll Task Queues and execute Workflow and Activity code. The Temporal Service never executes your code -- Workers do.

**Key characteristics:**
- Workers are **stateless**: blocked Workflow executions can be removed from memory and resurrected on any Worker
- A single Worker can handle millions of open Workflow executions
- Workers connect outbound to the Temporal Service (no inbound ports needed)
- Multiple Worker Entities can exist in one process, each listening to different Task Queues
- All Workers on a Task Queue **must register the same Workflows, Activities, and Nexus Operations**

**Worker identity** defaults to `${pid}@${hostname}` but should be customized in cloud environments (Docker containers always have PID 1, ECS has random hostnames).

### 5. Task Queues

Task Queues are **lightweight, dynamically allocated** queues that Workers poll for work. They are created on demand -- no explicit registration needed.

**Types:**
- **Workflow Task Queue** -- distributes Workflow Tasks
- **Activity Task Queue** -- distributes Activity Tasks
- **Nexus Task Queue** -- distributes Nexus Operation Tasks (sync-matched, not persisted)

**Partitioning:**
- Default: 4 partitions per Task Queue
- Single-partition: FIFO ordering, but limited throughput
- Multi-partition: Tasks randomly distributed across partitions (FIFO within each)

**Task routing** enables directing specific Activities to specialized Workers (e.g., GPU workers for ML inference, region-specific workers for data residency).

### 6. Signals, Queries, and Updates

Three mechanisms for communicating with running Workflows:

| Aspect | Signal | Query | Update |
|--------|--------|-------|--------|
| Direction | Write (async) | Read (sync) | Read/Write (sync) |
| Response | Fire-and-forget | Immediate result | Tracked result |
| History | Recorded | Never recorded | Recorded if accepted |
| Blocking | No | No | Yes (awaits completion) |
| Throughput | Unlimited | High | Limited (max 10 in-flight) |
| Can run Activities | Yes (async handler) | No | Yes (async handler) |

**When to choose:**
- **Signal**: Rapid async communication, no response needed (e.g., "cancel order", "approve request")
- **Query**: Efficient state reads without affecting history (e.g., "get order status")
- **Update**: Synchronous mutations with validation and result (e.g., "change language, return previous")

### 7. Child Workflows

A Child Workflow is spawned from within a parent Workflow in the same Namespace.

**When to use:**
- **Workload partitioning**: Each child has its own Event History, bypassing parent size limits. One parent can spawn ~1,000 children, each spawning ~1,000 Activities = 1M Activities total
- **Separate services**: Children can be processed by entirely different Worker sets
- **Resource representation**: Map one child per resource for serialized operations via unique IDs
- **Periodic logic**: Children can use Continue-As-New internally while appearing as single invocations to parents

**Parent Close Policy** governs what happens to children when the parent closes:
- `TERMINATE` (default) -- terminate children
- `REQUEST_CANCEL` -- request cancellation
- `ABANDON` -- let children continue independently

**Rule of thumb**: "When in doubt, use an Activity." Start with Activities until clear needs for Child Workflows emerge.

### 8. Continue-As-New

Continue-As-New closes the current Workflow Execution and starts a new one with the same Workflow ID but fresh Event History and a new Run ID. It is the mechanism for handling long-running workflows that would exceed history limits.

**When to use**: Check `workflowInfo().continueAsNewSuggested` or monitor `historyLength` against limits.

**Best practices:**
- Design workflow parameters to carry "current state" to the next execution
- Never call Continue-As-New from Signal or Update handlers -- wait for them to complete
- Children do not carry over when a parent uses Continue-As-New

### 9. Retry Policies

| Parameter | Default | Purpose |
|-----------|---------|---------|
| Initial Interval | 1 second | Wait before first retry |
| Backoff Coefficient | 2.0 | Exponential growth factor |
| Maximum Interval | 100s (100x initial) | Cap on retry delay |
| Maximum Attempts | Unlimited (0) | Total retry limit |
| Non-Retryable Errors | None | Error types that skip retries |

**Key principles:**
- Activities retry by default; Workflows do not (they should be deterministic)
- Use non-retryable errors for permanent failures (bad input, business rule violations)
- Prefer execution timeouts over max attempts to bound retry duration
- Only assign retry policies to Workflows for specific cases (stateless cron jobs, file processors)

### 10. Versioning

Temporal requires deterministic Workflow code. When code changes would break replay of in-progress Workflows, versioning provides safe migration paths.

**Approach 1: Patching (per-SDK)**

Three-step process (TypeScript example):
1. **Patch in new code**: `if (patched('my-change-id')) { /* new */ } else { /* old */ }`
2. **Deprecate patch**: Replace with `deprecatePatch('my-change-id')` after old Workflows complete
3. **Remove patch**: Deploy clean code after all patched Workflows exit retention

**Approach 2: Worker Versioning (GA as of March 2026)**

Workers tagged with Build IDs are assigned to version-specific Task Queue partitions. Workflows are pinned to the Worker version that started them, eliminating the need for patching. Includes "Upgrade on Continue-As-New" for migrating long-running Workflows to newer versions at checkpoint boundaries.

### 11. Temporal Nexus

Nexus enables **cross-namespace, cross-team orchestration** by exposing Workflow functionality as discoverable services.

**Architecture:**
- **Nexus Service**: Named collection of Operations abstracting underlying implementations
- **Nexus Endpoint**: Reverse proxy decoupling callers from handlers
- **Nexus Registry**: Manages Endpoints via UI, CLI, or Cloud Ops API

**Operation types:**
- **Asynchronous**: Start Workflows across Task Queues (up to 60-day duration)
- **Synchronous**: Complete within 10-second deadline (Signals, Queries, Updates)

**Built-in reliability:**
- Automatic retries with exponential backoff
- Rate and concurrency limiting
- Circuit breaking (after 5 consecutive errors)
- At-least-once execution semantics

**Multi-level composition**: `Workflow A -> Nexus Op 1 -> Workflow B -> Nexus Op 2 -> Workflow C`

### 12. Schedules

Schedules automate recurring Workflow executions, replacing the older cron workflow pattern.

**Features:**
- **Interval-based**: Every N seconds/minutes/hours
- **Calendar-based**: Specific days/times (e.g., "every Wednesday at 8:30 PM")
- **One-time**: Single future execution
- **Overlap policies**: Control behavior when executions would overlap (e.g., `ALLOW_ALL`, `SKIP`, `BUFFER_ONE`)
- **Backfill**: Execute missed past schedules retroactively
- **Pause/Resume**: Without affecting already-started Workflows

### 13. Visibility and Search Attributes

Visibility enables operators to view, filter, and search Workflow Executions.

**Default search attributes**: WorkflowType, WorkflowID, RunID, ExecutionStatus, StartTime, CloseTime

**Custom search attributes**: User-defined fields (strings, ints, booleans, datetimes, keyword lists) set at Workflow start or via `upsertSearchAttributes()`.

**Query syntax**: SQL-like filter language: `WorkflowType='OrderWorkflow' AND ExecutionStatus='Running'`

**Database support**: MySQL 8.0.17+, PostgreSQL 12+, Elasticsearch 7+, OpenSearch 2+.

---

## Server Architecture

The Temporal Service consists of the Temporal Server (written in Go) plus a database backend.

### Server Components

| Service | Role | Key Metrics |
|---------|------|-------------|
| **Frontend** | API gateway, rate limiting, routing, auth | 2,400 RPS/host, 2,000 persistence QPS |
| **History** | Manages Workflow state, event sourcing, timers, transfers | 3,000 RPS/host, 9,000 persistence QPS |
| **Matching** | Routes Tasks to Workers via Task Queues | 1,200 RPS/host, 4 partitions default |
| **Internal Worker** | System workflows (archival, replication, cleanup) | 100 persistence QPS |

### Database Backends

- **Cassandra**: Best for high-throughput, eventually consistent deployments
- **MySQL 8.0+**: RDBMS option with Advanced Visibility built in
- **PostgreSQL 12+**: RDBMS option with Advanced Visibility built in
- **SQLite**: Development only (in-memory via CLI `temporal server start-dev`)

### Scalability

- Workflows are bounded per instance; the system scales through multiple workflows, multiple hosts, and partitioned databases
- History Service uses **sharding** with Transfer Queues (transactional outbox) for consistency
- Tested to handle "beyond 200 million executions per second"

### Key Limits

| Resource | Warning | Error |
|----------|---------|-------|
| Event history size | 10 MB | 50 MB |
| Event count | 10,240 | 51,200 |
| Blob/payload size | 256 KB (warn), 512 KB | 2 MB |
| gRPC message | -- | 4 MB |
| Pending operations | 500 (recommended) | 2,000 |
| In-flight Updates | -- | 10 |
| Total Updates in history | -- | 2,000 |
| Identifier length | -- | 1,000 UTF-8 chars |

---

## SDK Reference

### TypeScript SDK

```typescript
// workflows.ts
import { proxyActivities } from '@temporalio/workflow';
import type * as activities from './activities';

const { greet } = proxyActivities<typeof activities>({
  startToCloseTimeout: '1 minute',
});

export async function example(name: string): Promise<string> {
  return await greet(name);
}
```

**TypeScript-specific features:**
- `proxyActivities<T>()` provides type-safe activity stubs
- Workflow sandbox isolates deterministic code
- `@temporalio/testing` package with time-skipping test environment
- Cancellation scopes for structured cleanup
- `condition()` for waiting on state with optional timeout
- Sinks for one-way data export from Workflow isolates

### Go SDK

```go
func Workflow(ctx workflow.Context, name string) (string, error) {
    ao := workflow.ActivityOptions{
        StartToCloseTimeout: time.Minute,
    }
    ctx = workflow.WithActivityOptions(ctx, ao)

    var result string
    err := workflow.ExecuteActivity(ctx, Activity, name).Get(ctx, &result)
    return result, err
}

func Activity(ctx context.Context, name string) (string, error) {
    logger := activity.GetLogger(ctx)
    logger.Info("Activity", "name", name)
    return "Hello " + name + "!", nil
}
```

**Go-specific features:**
- `workflow.Context` for deterministic operations
- `workflow.Go()` for coroutines within Workflows
- `workflow.NewSelector()` for select-like patterns on channels
- `workflow.GetSignalChannel()` for signal handling
- Session support for stateful Worker affinity
- Testify-based test suite with `WorkflowTestSuite`

### Python SDK

```python
from temporalio import activity, workflow
from temporalio.client import Client
from temporalio.worker import Worker
from dataclasses import dataclass
from datetime import timedelta

@dataclass
class ComposeGreetingInput:
    greeting: str
    name: str

@activity.defn
async def compose_greeting(input: ComposeGreetingInput) -> str:
    return f"{input.greeting}, {input.name}!"

@workflow.defn
class GreetingWorkflow:
    @workflow.run
    async def run(self, name: str) -> str:
        return await workflow.execute_activity(
            compose_greeting,
            ComposeGreetingInput("Hello", name),
            start_to_close_timeout=timedelta(seconds=10),
        )
```

**Python-specific features:**
- Decorator-based API: `@workflow.defn`, `@workflow.run`, `@activity.defn`
- Async-first with sync adapter support
- `@workflow.signal`, `@workflow.query`, `@workflow.update` decorators
- Dataclasses recommended for parameters (backward-compatible field additions)
- `asyncio.Lock` for concurrent handler safety
- Sandbox enforcement for deterministic constraints

### Java SDK

```java
@WorkflowInterface
public interface GreetingWorkflow {
    @WorkflowMethod
    String getGreeting(String name);
}

@ActivityInterface
public interface GreetingActivities {
    @ActivityMethod(name = "greet")
    String composeGreeting(String greeting, String name);
}

// Implementation
public class GreetingWorkflowImpl implements GreetingWorkflow {
    private final GreetingActivities activities =
        Workflow.newActivityStub(GreetingActivities.class,
            ActivityOptions.newBuilder()
                .setStartToCloseTimeout(Duration.ofSeconds(2))
                .build());

    public String getGreeting(String name) {
        return activities.composeGreeting("Hello", name);
    }
}
```

**Java-specific features:**
- Interface-based with `@WorkflowInterface`, `@WorkflowMethod`, `@ActivityInterface`, `@ActivityMethod`
- `@SignalMethod`, `@QueryMethod`, `@UpdateMethod`, `@UpdateValidatorMethod` annotations
- Spring Boot integration via `spring-boot-starter-temporal`
- `Workflow.newActivityStub()` for typed activity proxies
- Side effects API for non-deterministic one-off operations

### .NET SDK

**Key features:**
- Attribute-based: `[Workflow]`, `[WorkflowRun]`, `[Activity]`
- Async/await Task-based API
- NuGet package: `Temporalio`
- Code samples: github.com/temporalio/samples-dotnet

---

## Message Passing Across SDKs

### TypeScript

```typescript
// Define
const approve = wf.defineSignal<[ApproveInput]>('approve');
const getStatus = wf.defineQuery<string>('getStatus');
const setLanguage = wf.defineUpdate<Language, [Language]>('setLanguage');

// Register in workflow
wf.setHandler(approve, (input) => { approved = true; });
wf.setHandler(getStatus, () => currentStatus);
wf.setHandler(setLanguage, (lang) => { /* ... */ }, {
  validator: (lang) => { if (!supported(lang)) throw new Error('unsupported'); }
});

// Client usage
await handle.signal(approve, { name: 'me' });
const status = await handle.query(getStatus);
const prev = await handle.executeUpdate(setLanguage, { args: [Language.CHINESE] });
```

### Go

```go
// Signal via channel
signalChan := workflow.GetSignalChannel(ctx, "approve")
signalChan.Receive(ctx, &approveInput)

// Query handler
workflow.SetQueryHandler(ctx, "getStatus", func() (string, error) {
    return currentStatus, nil
})

// Update handler with validator
workflow.SetUpdateHandlerWithOptions(ctx, "setLanguage",
    func(ctx workflow.Context, lang Language) (Language, error) { /* ... */ },
    workflow.UpdateHandlerOptions{
        Validator: func(ctx workflow.Context, lang Language) error { /* ... */ },
    })
```

### Python

```python
@workflow.signal
async def approve(self, input: ApproveInput) -> None:
    self.approved = True

@workflow.query
def get_status(self) -> str:
    return self.current_status

@workflow.update
async def set_language(self, language: Language) -> Language:
    previous = self.language
    self.language = language
    return previous

@set_language.validator
def validate_language(self, language: Language) -> None:
    if language not in self.supported:
        raise ValueError(f"Unsupported: {language}")
```

### Java

```java
@SignalMethod
void approve(ApproveInput input);

@QueryMethod
String getStatus();

@UpdateMethod
Language setLanguage(Language language);

@UpdateValidatorMethod(updateName = "setLanguage")
void validateLanguage(Language language);
```

---

## Testing

### TypeScript

```typescript
import { TestWorkflowEnvironment } from '@temporalio/testing';

// Time-skipping environment
const env = await TestWorkflowEnvironment.createTimeSkipping();

// Activity mocking: provide mock implementations to Worker
// Replay testing: Worker.runReplayHistory() for CI/CD validation
```

### Go

```go
// testify-based suite
type UnitTestSuite struct {
    suite.Suite
    testsuite.WorkflowTestSuite
    env *testsuite.TestWorkflowEnvironment
}

// Time skipping, activity mocking, replay via WorkflowReplayer
```

### General Testing Strategy

- **Unit tests**: Test Activities in isolation with mocked contexts
- **Integration tests** (recommended majority): Workers with mocked Activities or real Temporal test server
- **Replay tests**: Verify Workflow determinism by replaying Event Histories from production -- critical for CI/CD when changing Workflow code

---

## Deployment

### Self-Hosted

**Requirements:**
- Temporal Server (single binary or separate services)
- Database: Cassandra, MySQL 8.0+, or PostgreSQL 12+
- Optional: Elasticsearch/OpenSearch for Advanced Visibility

**Options:**
- Docker Compose (development/staging)
- Kubernetes with Helm charts
- Bare metal with systemd

**Development server:** `temporal server start-dev` -- no dependencies, in-memory SQLite, Web UI at localhost:8233

### Temporal Cloud

| Plan | Monthly | Actions Included | P0 Support Response |
|------|---------|-----------------|---------------------|
| Essentials | $100 | 1M | 1 business day |
| Business | $500 | 2.5M | 2 hours |
| Enterprise | Contact sales | 10M | 30 minutes (24/7) |

**Volume pricing** scales from $50/M to $25/M for 200M+ actions.

**Free credits**: $1,000 for new users; $6,000 Startup Program for companies under $30M funding.

**Performance SLO**: p99 latency of 200ms per region. Actual observed (Jan 2026): StartWorkflow p99=69ms, Signal p99=46ms.

### Worker Deployment (AWS ECS Example)

- Stateless containers -- excellent for Fargate Spot (50-70% cost savings)
- No load balancer needed (outbound connections only)
- Health checks via lightweight HTTP endpoint
- Scale on CPU target ~25% (scale out before bottleneck)
- Asymmetric cooldowns: fast scale-out, slow scale-in
- Handle SIGTERM for graceful shutdown (30s drain period)

---

## Observability

### Metrics

All SDKs emit optional Prometheus or OpenTelemetry metrics:

**Critical metrics to monitor:**
- `service_requests` by operation and namespace
- `service_latency` percentiles for performance investigation
- `task_requests` / `task_errors` for History Service health
- `poll_success` / `poll_timeouts` for Worker-Task Queue connectivity
- `persistence_errors` / `persistence_latency` for database health
- `no_poller_tasks` for configuration mismatches

### Tracing

OpenTelemetry distributed tracing propagates across Workflows, Activities, Child Workflows, and Nexus Operations. SDKs provide interceptors for OpenTelemetry, OpenTracing, and Datadog.

### Logging

- Workflow loggers are replay-safe (avoid duplicate log messages during replay)
- Activity loggers include automatic execution metadata
- Runtime-level logger configuration via SDK options

### Web UI

The Temporal Web UI (localhost:8233 for dev, Temporal Cloud for managed) provides:
- Workflow execution search and filtering
- Event History timeline visualization
- Workflow state inspection
- Worker and Task Queue monitoring

---

## Security

- **mTLS**: Mutual TLS for all service communication (required for Temporal Cloud)
- **Namespace isolation**: Logical isolation boundaries within a Temporal Service
- **Data encryption**: Custom Payload Codecs encrypt data before it reaches the server; Temporal never sees unencrypted payloads
- **SAML/SCIM**: Enterprise SSO integration for Temporal Cloud
- **Audit logging**: Track account activities in Temporal Cloud
- **Security whitepaper**: Temporal Cloud provides "provable security by design"

---

## Common Pitfalls

| Pitfall | Why It Happens | How to Avoid |
|---------|---------------|--------------|
| Non-deterministic Workflow code | Using random, system time, or I/O in Workflows | Delegate to Activities; use SDK-provided alternatives |
| Event History growing too large | Long-running loops without Continue-As-New | Monitor `continueAsNewSuggested`; use Continue-As-New |
| Missing Start-To-Close timeout | Forgetting to set Activity timeouts | Always set Start-To-Close for production Activities |
| Unregistered handlers on Task Queue | Workers on same queue with different registrations | Ensure all Workers register identical Workflow/Activity sets |
| Sleeping in Activities | Using `time.sleep()` instead of Workflow timers | Use `workflow.sleep()` in Workflows; heartbeat in Activities |
| Non-idempotent Activities | Not handling retry scenarios | Design Activities to be safely re-executed |
| Blocking Query handlers | Making async calls or mutations in queries | Queries must be synchronous, read-only, non-blocking |
| Too many pending operations | Exceeding 2,000 concurrent child/activity/signal limits | Batch work; use child workflows for fan-out |
| Ignoring replay in logging | Workflows log during replay causing duplicates | Use SDK-provided replay-safe loggers |
| Patching mistakes | Incorrect versioning during Workflow code changes | Use replay tests in CI; follow three-step patching process |

---

## Best Practices

1. **Activities should be idempotent** -- they may be retried on failure. Use idempotency keys or check-before-write patterns.

2. **Keep Workflows small and focused** -- delegate all side-effects to Activities. Workflow code should only contain orchestration logic.

3. **Set Start-To-Close timeouts on all Activities** -- this is the most critical timeout for production reliability.

4. **Use Continue-As-New for long-running Workflows** -- check `continueAsNewSuggested` and design state to be passable as parameters.

5. **Prefer Schedules over cron Workflows** -- Schedules offer better management, backfill, and overlap control.

6. **Use replay testing in CI/CD** -- verify that Workflow code changes do not break determinism for in-flight executions.

7. **Design for observability** -- emit metrics, use custom search attributes, and leverage the Web UI for debugging.

8. **Use task queue routing** for specialized workloads -- separate GPU workers, region-specific workers, or different service teams.

9. **Start with Activities, graduate to Child Workflows** -- only use Child Workflows when you need separate history, different workers, or resource-level isolation.

10. **Handle cancellation gracefully** -- implement cleanup logic in finally blocks and use cancellation scopes for structured teardown.

11. **Use Update with validators** for mutations requiring acknowledgement -- prefer over Signal+Query polling patterns.

12. **Size your Workers appropriately** -- keep concurrent operations under 500 for optimal performance; scale horizontally.

---

## Use Cases

| Use Case | Example Companies | Pattern |
|----------|------------------|---------|
| Payment processing | Vinted (10-12M workflows/day) | Activity-level retries, saga compensation |
| AI agent orchestration | Replit, Retool, Gorgias | Long-running workflows, human-in-the-loop |
| Video/media processing | VEED.IO (4M daily workflows) | Activity-level retries for flaky AI APIs |
| Infrastructure provisioning | Netflix | Entity workflows, platform control planes |
| Order fulfillment | E-commerce platforms | Multi-step saga with compensation |
| Subscription management | SaaS platforms | Long-running, timer-based renewal workflows |
| Background checks | HR platforms | Multi-service orchestration |
| Data pipelines | Analytics platforms | Fan-out with child workflows |

---

## Temporal for AI

Temporal is emerging as the orchestration layer for AI agents:

- **Durable LLM calls**: Automatic retries for rate-limited or flaky model APIs
- **Agent state management**: Workflows maintain conversation history and tool state over extended sessions
- **Human-in-the-loop**: Signals/Updates for approval gates in agent decisions
- **Tool orchestration**: Activities wrap external tool calls (MCP, APIs) with retry and timeout guarantees
- **Parallel execution**: Multiple agent instances as concurrent Workflows
- **Observability**: Step-through debugging of agent reasoning and tool execution

---

## Further Reading

| Resource | Type | Why Recommended |
|----------|------|-----------------|
| [Temporal Documentation](https://docs.temporal.io/) | Official Docs | Comprehensive reference for all features |
| [Temporal TypeScript SDK Samples](https://github.com/temporalio/samples-typescript) | Code | Production-ready patterns and examples |
| [Temporal Go SDK Samples](https://github.com/temporalio/samples-go) | Code | Go-specific patterns including hello world, sagas, child workflows |
| [Temporal Python SDK Samples](https://github.com/temporalio/samples-python) | Code | Python async patterns with dataclass parameters |
| [Temporal Java SDK Samples](https://github.com/temporalio/samples-java) | Code | Interface-based Java patterns with Spring Boot |
| [Temporal 101 Courses](https://learn.temporal.io/) | Course | Free 2-hour courses in TS, Go, Java, Python |
| [Temporal Server GitHub](https://github.com/temporalio/temporal) | Source | Server architecture, 19.5k stars, MIT license |
| [Workflow Engine Principles Blog](https://temporal.io/blog/workflow-engine-principles) | Blog | Deep architectural insights into event sourcing and transactional consistency |
| [Building Reliable Distributed Systems in Node](https://temporal.io/blog/building-reliable-distributed-systems-in-node) | Blog | Practical patterns: signals, queries, conditions, saga compensation |
| [Deploying Workers to Amazon ECS](https://temporal.io/blog/deploying-temporal-workers-to-amazon-ecs) | Blog | Production deployment: health checks, scaling, cost optimization |
| [Worker Versioning GA Announcement](https://temporal.io/blog/announcing-ga-for-worker-versioning-and-public-preview-for-upgrade-on-continue-as-new) | Blog | Build ID versioning, upgrade-on-continue-as-new |
| [Temporal for AI](https://temporal.io/ai) | Landing Page | AI agent orchestration patterns and case studies |
| [Temporal Pricing](https://temporal.io/pricing) | Reference | Cloud pricing tiers, action-based model, free credits |
| [Platform Control Plane Blog](https://temporal.io/blog/why-your-platform-control-plane-belongs-on-temporal) | Blog | Entity workflows, infrastructure lifecycle management |
| [Temporal Community Forum](https://community.temporal.io/) | Community | Q&A, best practices, troubleshooting |
| [Temporal Slack](https://temporal.io/slack) | Community | Real-time help from Temporal engineers and community |

---

*This guide was synthesized from 42 sources. See `resources/temporal-sources.json` for full source metadata.*
