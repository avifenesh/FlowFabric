# Learning Guide: Restate -- Durable Execution Engine for Distributed Applications

**Generated**: 2026-04-09
**Sources**: 42 resources analyzed
**Depth**: deep

## Prerequisites

- Familiarity with distributed systems concepts (RPC, event-driven architecture, state machines)
- Experience with at least one supported language (TypeScript, Java/Kotlin, Python, Go, or Rust)
- Understanding of HTTP/2, REST APIs, and message brokers (Kafka helpful but not required)
- Basic knowledge of Docker for local development; Kubernetes for production deployment
- Awareness of failure modes in distributed systems (retries, idempotency, exactly-once semantics)

## TL;DR

- Restate is a lightweight durable execution engine (single Rust binary) that records every handler step in a journal, replaying completed steps on failure so your code runs to completion without re-executing side effects.
- Three service types: **Services** (stateless, unlimited parallelism), **Virtual Objects** (stateful per key, single-writer concurrency), and **Workflows** (exactly-once orchestration with durable promises).
- Built on a shared-log architecture (**Bifrost**) with partitioned processing, achieving ~3ms median step latency at low load and 94K actions/second at high load.
- SDKs for TypeScript, Java/Kotlin, Python, Go, and Rust -- services run as regular HTTP servers or serverless functions (Lambda, Cloud Run, Cloudflare Workers).
- Created by the founders of Apache Flink; addresses the gap between simple RPC handlers and complex workflow engines like Temporal, with significantly lower operational overhead.

## Core Concepts

### Durable Execution and the Journal

Restate tracks every step of handler execution in a persistent **journal**. When a handler calls `ctx.run()`, makes a service call, reads/writes state, or sleeps, both the action and its result are recorded. If the handler crashes or the process restarts, Restate replays the journal: completed steps are skipped (their cached results are used), and execution resumes from the first incomplete step.

This means:
- External API calls wrapped in `ctx.run()` are never re-executed on retry
- State updates are automatically journaled alongside execution steps
- Side effects are idempotent by construction, not by convention
- Timers survive restarts (if 8 of 12 hours elapsed, only 4 more are waited)

**Determinism requirement**: Code between durable operations must be deterministic. Non-deterministic operations (random numbers, UUIDs, timestamps) must use Restate-provided alternatives (`ctx.rand.uuidv4()`, `ctx.rand.random()`, `ctx.date.now()`) that produce consistent values seeded by the invocation ID.

### Service Types

**Services** are stateless, durable request handlers. They group related handlers as callable endpoints with unlimited parallel execution. Ideal for ETL pipelines, sagas, background jobs, and parallelized computation.

**Virtual Objects** are stateful entities identified by a unique key. State is isolated per key and retained indefinitely. They enforce a **single-writer per key** concurrency model: exclusive handlers execute sequentially for a given key, while shared handlers allow concurrent read-only access. Use cases include user accounts, shopping carts, chat sessions, AI agents, and state machines.

**Workflows** are multi-step orchestration processes. The `run` handler executes **exactly once per workflow ID**. Additional shared handlers run concurrently for signaling, querying state, or awaiting external events via durable promises. Use cases include approval flows, onboarding sequences, multi-step transactions, and complex orchestration.

| Aspect | Service | Virtual Object | Workflow |
|--------|---------|----------------|----------|
| State | None | Persistent K/V per key | Instance-scoped promises |
| Concurrency | Unlimited parallel | Single-writer per key | Single run + concurrent signals |
| Scaling | Horizontal | Per-key consistency | Per-ID execution |
| Lifetime | Per-request | Indefinite | Until retention expires |

### Awakeables

Awakeables are durable, distributed primitives for coordinating with external systems. A handler can create an awakeable (generating a unique ID and a promise), pass the ID to an external process (email service, webhook, human reviewer), and then **suspend** execution until the external process resolves or rejects it. The handler recovers from failures and resumes waiting automatically.

```typescript
const { id, promise } = ctx.awakeable<string>();
await ctx.run(() => requestHumanReview(name, id));
const review = await promise; // handler suspends here
```

Resolution can happen via SDK (`ctx.resolveAwakeable(id, data)`) or HTTP API.

### Durable Promises

Durable promises work within workflows and virtual objects to enable async coordination between the main `run` handler and shared interaction handlers. They allow external signals to be sent into a running workflow, which the workflow can await at specific points. Combined with awakeables, they form the foundation for human-in-the-loop patterns, approval workflows, and event-driven coordination.

### Service Communication

Handlers communicate through three patterns:

1. **Request-Response**: Synchronous call-and-wait via `ctx.serviceClient()` / `ctx.objectClient()`. All calls route through the Restate server (never direct service-to-service), preserving end-to-end idempotency.

2. **One-Way Messages**: Asynchronous fire-and-forget via `ctx.serviceSendClient()`. Restate manages delivery and retries. The calling handler completes without waiting.

3. **Delayed Messages**: Scheduled for future delivery with a time offset. Unlike sleeping then sending, the caller completes immediately and other requests to the same Virtual Object key can process during the delay.

**Deadlock warning**: Request-response calls between exclusive handlers on the same Virtual Object key (or circular cross-key calls) can deadlock. Use the CLI/UI to cancel blocked invocations.

### State Management

State is available in Virtual Objects and Workflows through Restate's embedded key-value store.

- **Get**: `ctx.get<T>(key)` -- returns null if key absent
- **Set**: `ctx.set(key, value)` -- persists JSON-serializable values
- **Clear**: `ctx.clear(key)` or `ctx.clearAll()`
- **List keys**: `ctx.stateKeys()`

Exclusive handlers can read and write; shared handlers are read-only. State loading modes: **eager** (default, loaded with request) or **lazy** (fetched on demand, reduces payload for large state).

### Error Handling

Restate distinguishes two error categories:

- **Transient errors**: Infrastructure issues (network, overload). Retried automatically with exponential backoff (infinite retries by default).
- **Terminal errors**: Business logic failures. Thrown explicitly (`throw new TerminalError(...)`) to halt retries and propagate up the call stack.

Retry policies are configurable at three levels: invocation, run-block, and context action. Parameters include `initial-interval`, `max-attempts`, `exponentiation-factor`, and `max-interval`. The `on-max-attempts` setting determines whether exhausted retries pause or kill the invocation.

Two timeouts govern handler execution:
- **Inactivity timeout** (default 1 minute): triggers graceful suspension when no journal entries arrive
- **Abort timeout** (default 10 minutes): forces termination after inactivity timeout

Both must be adjusted for long-running operations (e.g., LLM inference calls).

## Architecture Deep Dive

### Log-Centric Design (Bifrost)

Restate is built around **Bifrost**, a distributed replicated log that serves as the single source of truth. The core philosophy: "the log is the ground truth; everything else is transient state following the log."

Events flow: invocations, journal entries, and state updates persist to Bifrost first, then migrate to RocksDB indexes for fast access, with periodic snapshots to object storage (S3). This tiered approach delivers ~3ms median step latency at low load while maintaining cost-effective durability.

The log uses segmented virtual logs (inspired by LogDevice/Delos). Failover seals active segments and creates new ones with different leaders/replica sets, presenting a unified contiguous log externally.

### Partitioned Processing

Work is distributed across independent partitions. Each partition contains:
- A log partition (single sequencer ordering events, replicated across nodes)
- A partition processor (leader + optional followers)

All invocation-related data (idempotency checks, journal entries, state, futures) stays within one partition, eliminating cross-shard coordination. An out-of-band event shuffler routes inter-partition messages with exactly-once semantics.

### Server Roles

Nodes in a Restate cluster can assume specialized roles:
- **Metadata servers**: Raft-based consensus for cluster configuration
- **HTTP ingress nodes**: Handle external requests, route to correct partition
- **Log servers**: Durably persist events with quorum replication
- **Worker nodes**: Run partition processors in leader/follower mode

### Leader Failover

New processor leaders obtain monotonically increasing epochs. Upon taking leadership, they append an epoch-bump message to the log. Old leaders seeing this message step down at that exact log point. Messages carrying lower epochs are ignored, preventing split-brain and ensuring exactly-once commitment.

### Performance Characteristics

| Metric | Value |
|--------|-------|
| Median step latency (low load, 10 clients) | ~3ms |
| Sustained throughput (1200 clients, 9-step workflows) | 94,286 actions/sec (8,571 workflows/sec) |
| P50/P99 latency at high load | 116ms / 163ms |
| Workflow completion (10-step, p99) | <100ms |
| Cluster throughput (3-node) | 13,000 workflows/sec |

## SDK Reference

### TypeScript SDK

**Requirements**: Node.js >= v20.19, Bun, or Deno. Package: `@restatedev/restate-sdk` (latest v1.11.1).

```typescript
import * as restate from "@restatedev/restate-sdk";

const greeter = restate.service({
  name: "Greeter",
  handlers: {
    greet: async (ctx: restate.Context, name: string) => {
      const greeting = await ctx.run(() => generateGreeting(name));
      return greeting;
    },
  },
});

const counter = restate.object({
  name: "Counter",
  handlers: {
    increment: async (ctx: restate.ObjectContext) => {
      const count = (await ctx.get<number>("count")) ?? 0;
      ctx.set("count", count + 1);
      return count + 1;
    },
    get: restate.handlers.object.shared(
      async (ctx: restate.ObjectSharedContext) => {
        return (await ctx.get<number>("count")) ?? 0;
      }
    ),
  },
});

restate.serve({ services: [greeter, counter] });
```

**Deployment targets**: HTTP/2 server (port 9080), AWS Lambda, Bun, Deno, Cloudflare Workers, Vercel.

**Serialization**: JSON by default; supports Zod (Standard Schema), custom `Serde` implementations, and binary (`restate.serde.binary`).

**Testing**: `@restatedev/restate-sdk-testcontainers` runs a real Restate server in Docker for integration tests with state inspection.

### Java/Kotlin SDK

**Requirements**: JDK >= 17. Package: `sdk-java-http` / `sdk-kotlin-http` (latest v2.4.1). Annotation processor required (`sdk-api-gen` / `sdk-api-kotlin-gen`).

```java
@Service
public class Greeter {
    @Handler
    public String greet(Context ctx, String name) {
        String greeting = ctx.run(JsonSerdes.STRING,
            () -> generateGreeting(name));
        return greeting;
    }
}
```

- Spring Boot starters available (`sdk-spring-boot-starter`)
- Kotlin uses `kotlinx.serialization` by default; Java uses Jackson
- OpenTelemetry tracing with automatic context propagation
- Log4j2 integration with MDC support for invocation tracking

### Python SDK

**Requirements**: Python >= 3.10. Package: `restate_sdk[serde]` (latest v0.17.1).

```python
from restate import Service, Context

greeter = Service("Greeter")

@greeter.handler()
async def greet(ctx: Context, name: str) -> str:
    greeting = await ctx.run("generate", lambda: generate_greeting(name))
    return greeting
```

Communication via `ctx.service_call()`, `ctx.object_call()`, `ctx.service_send()`, `ctx.object_send()`.

### Go SDK

**Requirements**: Go >= 1.24.0. Package: `github.com/restatedev/sdk-go` (latest v0.24.0).

```go
type Greeter struct{}

func (g Greeter) Greet(ctx restate.Context, name string) (string, error) {
    greeting, err := restate.Run(ctx, func(ctx restate.RunContext) (string, error) {
        return generateGreeting(name), nil
    })
    return greeting, err
}
```

- State via `restate.Get[T]()`, `restate.Set()`, `restate.Clear()`
- Struct methods automatically converted to handlers via `restate.Reflect()`
- Protobuf-based code generation available
- Mock package for unit testing

### Rust SDK

**Requirements**: Rust stable. Package: `restate-sdk` (latest v0.9.0). **Active development, may break across releases.**

```rust
#[restate_sdk::service]
trait Greeter {
    async fn greet(name: String) -> HandlerResult<String>;
}

struct GreeterImpl;
impl Greeter for GreeterImpl {
    async fn greet(&self, ctx: Context<'_>, name: String) -> HandlerResult<String> {
        Ok(format!("Hello, {}!", name))
    }
}
```

Deploy via `HttpServer` or `LambdaEndpoint`.

## Deployment Model

### Self-Hosted Server

Restate ships as a single Rust binary with zero external dependencies. Install via Homebrew, npm, Docker, or direct binary download.

**Local development**: `restate-server` starts a single-node instance. Access the UI at `http://localhost:9070`. Use `restate up` for a quick dev server.

**Configuration** supports TOML files, environment variables (prefix `RESTATE_`, nested with `__`), and CLI arguments. Use `--dump-config` to inspect effective configuration.

### Cluster Deployment

Multi-node clusters use role-based node specialization (metadata, ingress, log-server, worker). The control plane uses Raft consensus. Partitions are distributed across nodes with quorum replication.

**Docker Compose**: Local cluster deployment for testing.
**Kubernetes**: Helm charts and a dedicated Restate Operator for automated deployment, service registration, traffic routing, and graceful draining during updates.

### Restate Cloud

Managed offering with SOC 2 compliance, Enterprise SSO, HIPAA readiness, RBAC, encryption, and cross-cloud deployment support (announced Feb 2026).

### Service Registration

Services register with the Restate server via CLI, UI, or Admin REST API:

```bash
restate deployments register http://localhost:9080
restate deployments list
restate deployments register --force localhost:9080  # dev mode
```

### Versioning Strategy

Restate uses **immutable deployments**: each version gets a unique endpoint. In-flight requests complete on their original version. New requests route to the latest version.

- FaaS platforms handle this naturally (Lambda versions, Vercel preview URLs)
- Kubernetes Operator automates registration and traffic draining
- Manual workflow: deploy new endpoint, register, monitor drain, remove old

**Breaking changes**: Use `--breaking` flag for handler removal/rename.
**Journal compatibility**: Avoid reordering/adding/removing SDK operations between versions for in-flight invocations.

## Operational Tooling

### CLI

- `restate invocation list` -- list active invocations
- `restate invocation cancel/kill` -- cancel by ID, service, handler, or object
- `restate kv clear` -- clear Virtual Object / Workflow state
- `restate sql` -- SQL introspection queries on invocation and state tables
- `restate service config view` -- inspect service configuration
- `restate deployments register/list/remove` -- manage deployments

### UI (localhost:9070)

Web dashboard for debugging, service management, invocation inspection, and a playground for testing handlers interactively.

### SQL Introspection

Query internal state via SQL for debugging:

| Table | Purpose |
|-------|---------|
| `sys_invocation` | Invocation status, lifecycle, timestamps |
| `sys_journal` | Individual journal entries per invocation |
| `state` | Service K/V state (per key) |
| `sys_service` | Registered services and types |
| `sys_deployment` | Endpoint registrations |
| `sys_keyed_service_status` | Active invocations per virtual object/workflow |
| `sys_promise` | Workflow promise state |
| `sys_idempotency` | Idempotency key mappings |
| `sys_inbox` | Pending invocations for keyed services |

### Observability

OpenTelemetry trace generation with automatic context propagation across service calls. Java SDK provides Log4j2 MDC integration for invocation-scoped logging.

### Invocation Control

- **Cancel**: Sends cancellation signal; surfaces as catchable exception for compensation logic
- **Pause/Resume**: Temporarily halt execution without failure; resume exactly where stopped
- **Restart**: Re-execute from scratch or from a specific step, preserving expensive prior work

## Patterns and Recipes

### Saga Pattern

Compensating transactions for distributed operations:

1. Execute business operations sequentially
2. Register compensating actions after each successful step
3. On terminal error, execute compensations in reverse order
4. Use two-phase APIs (reserve/confirm) or idempotency keys for safe retries

### Fan-Out / Fan-In

Distribute work across parallel subtasks, then aggregate:
- Schedule subtasks as one-way messages to separate handlers
- Collect durable promises for each subtask
- Use promise combinators to await all results
- Subtask handlers can run on serverless infrastructure for auto-scaling

### Rate Limiting

Token bucket algorithm implemented via Virtual Objects:
- Durable state tracks token count per key
- Durable timers schedule token refills
- Methods: `wait()`, `allow()`, `reserve()`, `setRate()`

### Cron Jobs

Recurring task scheduling using Virtual Objects:
- CronJob Virtual Object manages execution and rescheduling per job ID
- Standard cron expressions for scheduling
- Durable timers ensure accurate scheduling across restarts
- Job inspection and cancellation via API

### Durable Webhooks

Any Restate handler serves as a webhook endpoint:
- Exactly-once event delivery with automatic deduplication
- Guaranteed completion across failures and restarts
- Full access to durable primitives during processing

### Durable State Machines (XState)

The `restatedev/xstate` integration (v0.5.1) runs XState state machines on Restate, combining formal state machine definitions with durable execution guarantees.

### AI Agent Orchestration

Restate serves as a durable backbone for AI agents:
- LLM calls wrapped in `ctx.run()` are never re-executed on failure
- Tool calls are journaled, preventing duplicate side effects
- Agents can be paused, resumed, cancelled, and restarted
- Integrations with Vercel AI SDK, OpenAI Agents, Google ADK, Pydantic AI, and LangChain
- Langfuse integration for agent observability

## Kafka Integration

Restate natively consumes from Kafka topics, invoking corresponding handlers for each message. This enables event-driven architectures with exactly-once processing semantics, automatic retries, and durable state management for event handlers.

## Security

- **Private services**: Mark services as `private` to prevent direct HTTP access (still callable via SDK)
- **Request identity**: ED25519 cryptographic verification that requests originate from a specific Restate instance
- **Network isolation**: Only Restate needs to reach service endpoints; configure firewall rules accordingly
- **Client-side encryption**: Optional journal encryption for inputs/outputs, step results, and state

## Comparison with Temporal

| Aspect | Restate | Temporal |
|--------|---------|---------|
| **Architecture** | Log-centric (Bifrost), single Rust binary | Database-backed (Cassandra/MySQL/PostgreSQL) |
| **Operational complexity** | Single binary, zero dependencies | Requires database cluster + multiple services |
| **Programming model** | Standard RPC handlers with context | Workflow/Activity separation with specific constraints |
| **State management** | Built-in K/V store per Virtual Object | External state or workflow variables |
| **Serverless support** | Native (Lambda, Cloud Run, Workers) | Limited (requires long-running workers) |
| **Service types** | Services, Virtual Objects, Workflows | Workflows + Activities |
| **Concurrency model** | Per-key single-writer (Virtual Objects) | Per-workflow task queue |
| **Latency** | ~3ms median step (low load) | Higher due to DB round-trips |
| **Language SDKs** | TS, Java/Kotlin, Python, Go, Rust | Go, Java, Python, TypeScript, PHP, .NET |
| **Maturity** | v1.6 (Feb 2026), ~3.7K GitHub stars | v1.x (2020+), ~12K+ GitHub stars |
| **FaaS integration** | First-class (suspension, auto-scaling) | Requires dedicated worker processes |
| **Founded by** | Apache Flink creators | Uber Cadence team |

**Key differentiators**: Restate is lighter operationally (single binary vs. database cluster), integrates natively with serverless, and uses a log-centric design that eliminates distributed coordination. Temporal has a larger ecosystem, more mature tooling, and broader language support.

## Upgrading Restate

- **Patch versions**: Always safe to upgrade
- **Minor versions**: Must be upgraded incrementally (no skipping)
- **Rollback**: Safe from `x.y.0` to latest `x.(y-1).z` if no version-exclusive features used
- **SDKs version independently**: Check protocol version compatibility
- Always capture effective configuration and back up data before upgrading

## Common Pitfalls

| Pitfall | Why It Happens | How to Avoid |
|---------|---------------|--------------|
| Non-deterministic code between durable operations | Random values, system time, or environment-dependent logic between `ctx.run()` calls | Use `ctx.rand.*` and `ctx.date.now()` for deterministic alternatives |
| Deadlocks in Virtual Objects | Request-response calls between exclusive handlers on the same key or circular cross-key calls | Use one-way messages or shared handlers; monitor with CLI/UI |
| Journal mismatch (RT0016) after code update | Reordering, adding, or removing SDK operations between versions while invocations are in-flight | Use immutable deployments; pause/resume for fixes |
| Forgetting `ctx.run()` for side effects | Database writes or API calls made outside `ctx.run()` are re-executed on replay | Wrap ALL non-deterministic operations in `ctx.run()` |
| Inactivity timeout killing long operations | Default 1-minute inactivity timeout suspends handler during long LLM calls or API waits | Increase `inactivity_timeout` and `abort_timeout` in service config |
| State schema changes breaking Virtual Objects | State persists indefinitely; incompatible schema changes corrupt existing objects | Maintain backward-compatible state schemas |
| Terminal errors without compensation | Throwing TerminalError after successful side effects leaves inconsistent state | Implement saga pattern with compensation actions |
| Cloudflare Workers minification | SDK breaks when minified | Set `minify = false` in `workers.toml` |

## Best Practices

1. **Wrap all external calls in `ctx.run()`**: Every HTTP request, database query, or non-deterministic operation must be journaled. (Source: docs.restate.dev)
2. **Use Virtual Objects for entity state**: Prefer Restate's built-in K/V store over external databases when possible, to keep state consistent with execution. (Source: docs.restate.dev)
3. **Prefer one-way messages over request-response for Virtual Objects**: Avoids deadlock risk and allows concurrent processing. (Source: docs.restate.dev)
4. **Implement saga compensation for multi-step mutations**: Register undo actions after each successful step; execute in reverse on failure. (Source: docs.restate.dev/guides/sagas)
5. **Use immutable deployments for production**: Each version gets a unique endpoint; never modify code under an existing endpoint URL. (Source: docs.restate.dev/operate/versioning)
6. **Adjust timeouts for long operations**: Set `inactivity_timeout` and `abort_timeout` appropriately for LLM calls, large file processing, etc. (Source: docs.restate.dev)
7. **Use idempotency keys for external calls**: Prevent duplicate processing when callers retry requests to Restate. (Source: docs.restate.dev)
8. **Test with Testcontainers**: Run integration tests against a real Restate server in Docker rather than mocking the context. (Source: docs.restate.dev)
9. **Monitor with SQL introspection**: Query `sys_invocation` and `sys_journal` tables for debugging stuck or failed invocations. (Source: docs.restate.dev)
10. **Design for deterministic replay**: Never rely on wall-clock time, random values, or environment state between durable operations. (Source: docs.restate.dev)

## Further Reading

| Resource | Type | Why Recommended |
|----------|------|-----------------|
| [Restate Documentation](https://docs.restate.dev) | Official Docs | Comprehensive reference for all concepts, SDKs, and deployment |
| [Building a Modern Durable Execution Engine from First Principles](https://restate.dev/blog/building-a-modern-durable-execution-engine-from-first-principles/) | Blog | Deep dive into Bifrost log architecture and design decisions |
| [Why We Built Restate](https://restate.dev/blog/why-we-built-restate/) | Blog | Founding story, philosophy, and comparison with existing solutions |
| [Restate GitHub](https://github.com/restatedev/restate) | Source | Server source code, issues, releases (Rust) |
| [Restate Examples](https://github.com/restatedev/examples) | Examples | 25+ patterns across all SDKs with end-to-end applications |
| [TypeScript SDK](https://github.com/restatedev/sdk-typescript) | SDK | Primary SDK with latest features (v1.11.1) |
| [Java/Kotlin SDK](https://github.com/restatedev/sdk-java) | SDK | JVM SDK with Spring Boot integration (v2.4.1) |
| [Go SDK](https://github.com/restatedev/sdk-go) | SDK | Go SDK with protobuf codegen (v0.24.0) |
| [Python SDK](https://github.com/restatedev/sdk-python) | SDK | Python SDK with async support (v0.17.1) |
| [Rust SDK](https://github.com/restatedev/sdk-rust) | SDK | Rust SDK, active development (v0.9.0) |
| [XState Integration](https://github.com/restatedev/xstate) | Integration | Durable state machines on Restate (v0.5.1) |
| [Restate Cloud](https://cloud.restate.dev) | Managed Service | SOC 2, HIPAA, Enterprise SSO managed deployment |
| [Error Handling Guide](https://docs.restate.dev/guides/error-handling) | Guide | Transient vs terminal errors, retry policies, compensation |
| [Saga Pattern Guide](https://docs.restate.dev/guides/sagas) | Guide | Compensating transactions with code examples |
| [Versioning Guide](https://docs.restate.dev/operate/versioning) | Guide | Immutable deployments, safe updates, journal compatibility |

---

*This guide was synthesized from 42 sources. See `resources/restate-sources.json` for full source list with quality scores.*
