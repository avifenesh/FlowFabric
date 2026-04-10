# Learning Guide: Inngest - Event-Driven Durable Workflow Engine

**Generated**: 2026-04-09
**Sources**: 40 resources analyzed
**Depth**: deep

## Prerequisites

- Familiarity with at least one of: TypeScript/JavaScript, Python, or Go
- Understanding of HTTP request/response model
- Basic knowledge of async programming (Promises, async/await)
- Familiarity with serverless concepts is helpful but not required
- Node.js 18+ or Python 3.10+ or Go 1.21+ installed

## TL;DR

- Inngest is a durable workflow engine that lets you write reliable multi-step functions triggered by events, cron schedules, or webhooks -- without managing queues, state, or infrastructure.
- Each `step.run()` call is a retriable checkpoint: completed steps are memoized and never re-executed on retry. Each step executes as a separate HTTP request.
- Flow control primitives (concurrency, throttle, debounce, rate-limit, priority, batching, singleton) are declared in function configuration, not coded.
- SDKs exist for TypeScript/JavaScript, Python, Go, and Kotlin/Java. Cross-language function invocation works via `step.invoke()`.
- Self-hosting is supported via a single binary (CLI, Docker, Helm chart) backed by Redis + PostgreSQL/SQLite.

---

## 1. What Is Inngest?

Inngest is a workflow orchestration platform that provides **durable execution** for application code. It replaces the need to manage separate queues, state stores, and retry infrastructure by wrapping your code in a step-based execution model that automatically handles failures, state persistence, and flow control.

Key characteristics:
- **Event-driven**: Functions trigger from application events, cron schedules, webhooks, or direct invocation
- **Durable**: Every step output is persisted; functions resume from where they left off after failures
- **Infrastructure-agnostic**: Works on serverless (Vercel, Netlify, AWS Lambda), edge, and traditional servers
- **Multi-tenant aware**: Built-in flow control scoped per user, account, or any custom key

Trusted in production by Replit, SoundCloud, Cohere, TripAdvisor, Resend, and ElevenLabs. Handles 100K+ executions per second at scale.

### Cloud vs Self-Hosted

Inngest is available as:
1. **Inngest Cloud** -- managed SaaS with SOC 2 Type II compliance, SAML, encryption at rest
2. **Self-hosted** -- open-source server (SSPL license, converting to Apache 2.0 after 3 years; SDKs are Apache 2.0)

---

## 2. Architecture

### System Components

```
Events (HTTP) --> [Event API] --> [Event Stream] --> [Runner]
                                                       |
                                          +------------+------------+
                                          |                         |
                                      [Queue]                 [Executor]
                                   (flow control)          (runs functions)
                                          |                         |
                                    [State Store]            [Your App]
                                   (Redis/memory)         (SDK via HTTP)
                                          |
                                    [Database]
                                  (Postgres/SQLite)
                                          |
                                  [API + Dashboard]
```

| Component | Role |
|-----------|------|
| **Event API** | Receives events via HTTP, authenticates with Event Keys |
| **Event Stream** | Internal buffer between API and Runner |
| **Runner** | Consumes events, schedules function runs, handles cancellations and waitForEvent resumptions |
| **Queue** | Multi-tenant queue supporting concurrency, throttling, debouncing, rate limiting, batching, priority |
| **Executor** | Invokes functions via HTTP, manages step execution and retries |
| **State Store** | Persists step outputs and run data (Redis or in-memory) |
| **Database** | Stores system data, history, apps, functions, events (PostgreSQL or SQLite) |
| **API + Dashboard** | GraphQL/REST APIs and web UI for management and observability |

### The Execution Model: Memoization, Not Replay

Unlike Temporal's deterministic replay model (which re-executes the entire workflow history), Inngest uses **step-based memoization**:

1. **Initial call**: The function handler runs. When it encounters the first `step.run()`, the SDK executes the callback, captures the result, and halts.
2. **State persistence**: The step's ID is hashed as the state identifier. The result is stored externally.
3. **Subsequent calls**: On the next invocation, the function re-initializes with event data and previously persisted state. Completed steps return cached outputs instantly; the next unfinished step executes.

**Critical design detail**: Each step in a function executes as a **separate HTTP request**. This enables execution across different infrastructure instances and supports serverless environments naturally.

This means:
- No determinism requirements -- use any language primitive (Date.now(), Math.random())
- No replay overhead -- completed work is never re-executed
- Each step has independent retry counters
- Functions can run across different machines between steps

### Checkpointing

Inngest supports **checkpointing** -- an optimization where steps run back-to-back without waiting for state persistence between them. State commits asynchronously, reducing inter-step latency from tens-of-milliseconds to single-digit milliseconds. If a failure occurs, the system replays only from the last checkpoint.

Checkpointing is enabled by default in TypeScript SDK v4.

---

## 3. Events and Triggers

### Event Schema

```typescript
{
  name: "app/user.signup",      // Required: lowercase dot notation
  data: {                        // Required: JSON-serializable payload
    userId: "usr_123",
    email: "user@example.com"
  },
  user: {                        // Optional: encrypted at rest
    external_id: "usr_123"
  },
  id: "evt_unique_123",         // Optional: for idempotent triggering
  ts: 1712678400000,            // Optional: millisecond timestamp; future = delayed execution
  v: "2024-04-09"               // Optional: payload version
}
```

### Sending Events

```typescript
// From application code
await inngest.send({
  name: "app/user.signup",
  data: { userId: "usr_123", plan: "pro" }
});

// Batch sending (up to 512KB total payload)
await inngest.send([
  { name: "app/invoice.created", data: { invoiceId: "inv_1" } },
  { name: "app/invoice.created", data: { invoiceId: "inv_2" } },
]);

// From within a function (preferred -- includes tracing context)
await step.sendEvent("send-welcome", {
  name: "app/welcome.send",
  data: { userId: event.data.userId }
});
```

### Trigger Types

| Trigger | Description | Example |
|---------|-------------|---------|
| **Event** | Fires when a matching event is received | `{ event: "app/user.signup" }` |
| **Cron** | Fires on a schedule | `{ cron: "0 9 * * MON" }` |
| **Cron + TZ** | Timezone-aware schedule | `{ cron: "TZ=Europe/Paris 0 12 * * 5" }` |

Functions can have **multiple triggers** to handle different event types in one function.

### Event Filtering

Use CEL (Common Expression Language) expressions to filter which events trigger a function:

```typescript
triggers: {
  event: "app/order.created",
  if: "event.data.total > 100"
}
```

---

## 4. Functions and Steps

### Creating a Function

```typescript
import { Inngest } from "inngest";

const inngest = new Inngest({ id: "my-app" });

export const processOrder = inngest.createFunction(
  {
    id: "process-order",
    triggers: { event: "app/order.created" },
    retries: 4,
  },
  async ({ event, step, runId, logger, attempt }) => {
    // Function body with steps
  }
);
```

### Handler Arguments

| Argument | Type | Description |
|----------|------|-------------|
| `event` | object | The triggering event payload |
| `events` | array | All events in batch (when batching configured) |
| `step` | object | Step API methods |
| `runId` | string | Unique execution identifier |
| `logger` | object | Structured logger (info, warn, error, debug) |
| `attempt` | number | Zero-indexed retry count |

### Step Methods

#### step.run() -- Execute Retriable Code

```typescript
const user = await step.run("fetch-user", async () => {
  return await db.users.findById(event.data.userId);
});
// `user` is available in subsequent steps via memoization
```

- Return values are serialized as JSON
- Each step has independent retry counters
- Errors trigger automatic retries per configuration

#### step.sleep() -- Pause Execution

```typescript
await step.sleep("wait-24h", "24 hours");
await step.sleep("wait-ms", 30 * 60 * 1000);
await step.sleep("wait-temporal", Temporal.Duration.from({ minutes: 30 }));
```

- Supported formats: milliseconds, ms-compatible strings ("30m", "2.5d", "3 hours"), Temporal.Duration
- Zero resource consumption during sleep

#### step.waitForEvent() -- Wait for External Signal

```typescript
const approval = await step.waitForEvent("wait-for-approval", {
  event: "app/invoice.approved",
  timeout: "7d",
  match: "data.invoiceId",  // Correlate by field
});

if (approval === null) {
  // Timeout reached -- no matching event received
} else {
  // Process approval event
}
```

- Returns the matched event payload, or `null` on timeout
- `match`: Dot-notation property path for correlation
- `if`: CEL expression for complex matching: `"event.data.userId == async.data.userId && async.data.plan == 'pro'"`

#### step.invoke() -- Call Another Function

```typescript
const result = await step.invoke("compute", {
  function: computeSquare,  // Direct reference or function ID
  data: { number: 4 },
  timeout: "1h",            // Default: 1 year
});
```

- Returns the invoked function's return value
- Failures throw `NonRetriableError` (prevents retry compounding)
- Enables cross-language function calls (TS calling Python, etc.)

#### step.sendEvent() -- Emit Events Durably

```typescript
const { ids } = await step.sendEvent("fan-out", [
  { name: "app/process.chunk", data: { chunk: 1 } },
  { name: "app/process.chunk", data: { chunk: 2 } },
]);
```

- Preferred over `inngest.send()` inside functions (adds tracing context)
- Returns array of Event IDs

### Parallel Step Execution

```typescript
// TypeScript: Don't await individual steps, then Promise.all
const emailTask = step.run("send-email", () => sendEmail(user));
const dbTask = step.run("update-db", () => db.update(user));
const [emailId, dbResult] = await Promise.all([emailTask, dbTask]);
```

```python
# Python: Use ctx.group.parallel()
results = await ctx.group.parallel([
    lambda: ctx.step.run("send-email", send_email),
    lambda: ctx.step.run("update-db", update_db),
])
```

Constraints:
- Maximum **4MB** total data from all parallel steps
- Maximum **1,000 steps** per function
- Each step retries independently

---

## 5. Flow Control

All flow control is declared in the function configuration object -- no code changes needed.

### Concurrency

Limits the number of **actively executing steps** (not paused/sleeping runs).

```typescript
// Basic: limit to 10 concurrent steps
concurrency: 10,

// Keyed: per-tenant virtual queues
concurrency: [{
  key: "event.data.account_id",
  limit: 10,
}],

// Multiple scopes (max 2 constraints)
concurrency: [
  { scope: "account", key: '"openai"', limit: 60 },
  { scope: "fn", key: "event.data.account_id", limit: 1 },
],
```

| Scope | Behavior |
|-------|----------|
| `fn` (default) | Per-function limit |
| `env` | Shared across functions in same environment with matching key |
| `account` | Shared globally across all environments with matching key |

FIFO ordering within same function. Creates virtual queues per unique key value.

### Throttling

Controls how many function runs **start** within a time period. Excess runs are **enqueued** (not dropped) in FIFO order.

```typescript
throttle: {
  limit: 1,           // Max runs per period
  period: "5s",       // 1s to 7 days
  burst: 2,           // Extra allowance in single window
  key: "event.data.user_id",  // Optional: per-entity
}
```

Uses the **Generic Cell Rate Algorithm (GCRA)**. Ideal for navigating third-party API rate limits.

### Rate Limiting

Hard constraint that **skips** (drops) events beyond the limit. Events are NOT queued.

```typescript
rateLimit: {
  limit: 1,           // Max executions per period
  period: "4h",       // Max 24 hours
  key: "event.data.company_id",
}
```

Uses GCRA with time buckets: `limit / period = bucket window`. Example: 10 limit over 60 minutes = 6-minute buckets.

### Debounce

Delays execution until events stop arriving. Executes with the **most recent** event.

```typescript
debounce: {
  period: "5m",        // Window that resets on each new event
  key: "event.data.account_id",
  timeout: "10m",      // Max wait before forced execution
}
```

Period range: 1 second to 7 days. Incompatible with batching.

### Priority

Dynamically reorder queued function runs. Expression evaluates to seconds (-600 to +600).

```typescript
priority: {
  run: "event.data.account_type == 'enterprise' ? 120 : 0"
}
```

Positive values advance execution; negative values delay. Most effective when combined with concurrency controls (priority matters when jobs are waiting).

### Batching

Process multiple events in a single function run.

```typescript
batchEvents: {
  maxSize: 100,        // Max events per batch
  timeout: "60s",      // Max wait for batch to fill
  key: "event.data.tenant_id",  // Optional: batch per key
  if: "event.data.priority == 'low'",  // Optional: conditional
}
```

- Function receives `events` array instead of single `event`
- Hard limit: 10 MiB max batch size
- Incompatible with: idempotency, rate limiting, cancellation events, priority

### Singleton

Ensures only one run at a time (optionally per key).

```typescript
singleton: {
  key: "event.data.user_id",
  mode: "skip",    // "skip" = discard new, "cancel" = cancel existing
}
```

Compatible with debounce, rate limiting, throttling. Incompatible with batching.

### Flow Control Comparison

| Mechanism | Excess Events | Use Case |
|-----------|--------------|----------|
| **Concurrency** | Queued (waits) | Limit active parallel work |
| **Throttle** | Queued (delayed) | Smooth out bursts, respect API limits |
| **Rate Limit** | Dropped (skipped) | Prevent abuse, hard cap |
| **Debounce** | Replaced (latest wins) | Noisy webhooks, rapid updates |
| **Priority** | Reordered | Premium customers, critical work |
| **Batching** | Grouped | Bulk DB writes, reduce API calls |
| **Singleton** | Skipped or cancels existing | Data sync, one-at-a-time jobs |

---

## 6. Error Handling and Retries

### Default Behavior

- **4 retries** beyond the initial attempt (5 total attempts by default)
- **Exponential backoff with jitter** between retries
- Each `step.run()` has its **own independent retry counter**
- A function with 3 steps and 4 retries could theoretically make 15 total attempts (5 per step)

### Configuration

```typescript
inngest.createFunction({
  id: "my-fn",
  retries: 10,          // 0 to 20
  triggers: { event: "app/action" },
}, async ({ event, step, attempt }) => {
  console.log(`Attempt ${attempt}`);  // Zero-indexed
});
```

### Special Error Types

```typescript
import { NonRetriableError, RetryAfterError } from "inngest";

// Permanent failure -- skip remaining retries
throw new NonRetriableError("User not found", { cause: originalError });

// Custom retry timing (e.g., from rate-limit headers)
throw new RetryAfterError("Rate limited", retryAfterSeconds);
```

### Failure Handlers

```typescript
inngest.createFunction({
  id: "update-subscription",
  retries: 5,
  onFailure: async ({ event, error }) => {
    // Runs after ALL retries exhausted
    await notifyTeam(event.data.userId, error);
  },
  triggers: { event: "user/subscription.check" },
}, async ({ event }) => { /* ... */ });
```

Also available: Listen for `inngest/function.failed` system event environment-wide.

### Idempotency Requirement

Steps may re-execute on retry. Code must be idempotent:
- Use upserts instead of inserts
- Use idempotency keys for payment APIs
- Design operations that are safe to repeat

---

## 7. Cancellation

### Event-Based Cancellation

```typescript
inngest.createFunction({
  id: "send-reminder",
  cancelOn: [{
    event: "app/reminder.cancelled",
    match: "data.reminderId",
  }],
  triggers: { event: "app/reminder.scheduled" },
}, async ({ event, step }) => {
  await step.sleep("wait", "24h");
  await step.run("send", () => sendPush(event.data));
});
```

- Active steps run to completion; cancellation prevents the **next** step
- Does NOT prevent new runs from being enqueued (use Function Pausing for that)
- System event `inngest/function.cancelled` fires for cleanup

### Other Cancellation Methods

- **Dashboard**: Bulk cancel runs within a timeframe
- **REST API**: Programmatically cancel individual or batch runs

---

## 8. Fan-Out Patterns

### Event-Based Fan-Out (One-to-Many)

Multiple functions subscribe to the same event:

```typescript
// Function A
inngest.createFunction(
  { id: "send-welcome-email", triggers: { event: "app/user.signup" } },
  async ({ event }) => { /* send email */ }
);

// Function B
inngest.createFunction(
  { id: "create-stripe-trial", triggers: { event: "app/user.signup" } },
  async ({ event }) => { /* create trial */ }
);

// Function C
inngest.createFunction(
  { id: "add-to-crm", triggers: { event: "app/user.signup" } },
  async ({ event }) => { /* add to CRM */ }
);
```

All three run in parallel independently when `app/user.signup` fires.

Benefits:
- Each function retries independently
- Add/remove functions without touching others
- Test in isolation
- Works across codebases and languages

### Step-Based Parallelism (Within a Function)

For coordinated work where you need all results:

```typescript
const chunks = splitData(largeDataset, 10);
const results = await Promise.all(
  chunks.map((chunk, i) =>
    step.run(`process-${i}`, () => processChunk(chunk))
  )
);
const combined = mergeResults(results);
```

### Fan-Out vs Parallelism

| Aspect | Fan-Out | Parallelism |
|--------|---------|-------------|
| Scope | Separate functions | Within one function |
| State sharing | Via events only | Direct access to step outputs |
| Retry isolation | Full (per function) | Per step within function |
| Scalability | Unlimited | Max 1,000 steps, 4MB data |

---

## 9. Middleware

### Overview

Middleware executes at various lifecycle points: function execution and event transmission. Available in TypeScript SDK v4+ and Python SDK.

### Execution Order

1. Client-level middleware (descending registration order)
2. Function-level middleware (descending registration order)

### Pre-Built Middleware

| Middleware | Purpose |
|------------|---------|
| **Encryption** | End-to-end encryption for events, step outputs, function results |
| **Sentry** | Error tracking integration |
| **Datadog** | Distributed tracing |
| **Dependency Injection** | Share client instances (DB, API) across functions |
| **Cloudflare Workers/Hono** | Environment variable access |

### TypeScript SDK v4 Middleware

v4 introduced class-based middleware with instance state per request:
- **Observable hooks**: Logging, metrics
- **Wrapping hooks**: Before/after logic
- **Transform hooks**: Modify inputs/outputs

### Creating Custom Middleware

Middleware is created as a function returning lifecycle hooks. Initialize dependencies within initializer functions rather than globally. The recommended pattern takes configuration options and returns the middleware.

---

## 10. Replay and Recovery

### What Is Replay?

Replay re-runs failed function executions from a specified time window. It operates at the event level -- the original events are re-sent to trigger fresh function runs.

### Workflow

1. Identify the bug causing failures
2. Deploy the fix
3. Use Replay to reprocess affected runs

### Configuration

From the dashboard:
- Specify a descriptive name (e.g., "Bug fix from PR #958")
- Select a time range
- Filter by status (Failed, Succeeded, or both)

### Key Behavior

- Runs are **spread over time** to avoid overwhelming your application
- Progress is tracked on a dedicated Replay page
- Useful for: bug recoveries, external outage recovery, DST-related issues

---

## 11. SDK Reference

### TypeScript/JavaScript SDK (v4)

```bash
npm install inngest
```

**Client setup:**
```typescript
import { Inngest } from "inngest";
const inngest = new Inngest({ id: "my-app" });
```

**Serving functions (framework-specific):**
```typescript
// Next.js
import { serve } from "inngest/next";
export default serve({ client: inngest, functions: [fn1, fn2] });

// Express
import { serve } from "inngest/express";
app.use("/api/inngest", serve({ client: inngest, functions: [fn1, fn2] }));
```

**Supported frameworks**: Next.js (App Router + Pages Router), Express, Astro, Bun, Fastify, Koa, NestJS, Nuxt, Remix, SvelteKit, AWS Lambda, Hono.

**Serve endpoint behavior**:
- GET: Returns function metadata / dev landing page
- POST: Invokes functions
- PUT: Registers functions with Inngest (uses signing key)

**v4 key improvements**:
- Checkpointing enabled by default (single-digit ms inter-step latency)
- Optimized parallelism by default (fewer HTTP round trips)
- Class-based middleware with per-request instance state
- `eventType()` helper for type-safe triggers with optional Zod validation
- Pino-style structured logging with pluggable logger support

### Python SDK

```bash
pip install inngest
```

**Requirements**: Python 3.10+

**Framework support**: Flask (3.0+), FastAPI (0.110+), Django (5.0+), Tornado (6.4+), DigitalOcean Functions.

```python
import inngest

inngest_client = inngest.Inngest(app_id="my-app")

@inngest_client.create_function(
    fn_id="process-order",
    trigger=inngest.TriggerEvent(event="app/order.created"),
)
async def process_order(ctx: inngest.Context) -> str:
    result = await ctx.step.run("fetch-data", fetch_data)
    return f"Processed: {result}"
```

**Features**: Sync and async functions, `step.run()`, `step.sleep()`, parallel via `ctx.group.parallel()`.

### Go SDK

```bash
go get github.com/inngest/inngestgo
```

```go
client := inngestgo.NewClient(inngestgo.ClientOpts{AppID: "my-app"})

fn := inngestgo.CreateFunction(
    inngestgo.FunctionOpts{ID: "process-order"},
    inngestgo.EventTrigger("app/order.created"),
    func(ctx context.Context, input inngestgo.Input[MyEvent]) (any, error) {
        result, err := step.Run(ctx, "fetch-data", func(ctx context.Context) (string, error) {
            return fetchData(input.Event.Data.OrderID)
        })
        return result, err
    },
)
```

**Features**: Type-safe generics, `step.Run()`, `step.Sleep()`, `step.WaitForEvent[]`, declarative flow control.

### Kotlin/Java SDK

Available as `inngest-kt`. Community-supported.

### Cross-Language Invocation

```typescript
// TypeScript calling a Python function
const result = await step.invoke("call-python", {
  function: referenceFunction({ functionId: "python-compute" }),
  data: { values: [1, 2, 3] },
});
```

The SDK protocol is HTTP-based: state resides in Inngest infrastructure, SDKs handle request/response chains. This decoupled model makes cross-language calls natural.

---

## 12. Self-Hosting

### Quick Start

```bash
# Via npm
npm install -g inngest-cli
inngest start --event-key my-event-key --signing-key $(openssl rand -hex 32)

# Via Docker
docker run -p 8288:8288 -p 8289:8289 inngest/inngest \
  inngest start --event-key my-event-key --signing-key $(openssl rand -hex 32)
```

**Default ports**: 8288 (Event API + Dashboard + API), 8289 (Connect gateway)

### Backing Services

| Service | Default | Production |
|---------|---------|------------|
| Queue/State | In-memory Redis | External Redis (`--redis-uri`) |
| Database | SQLite (`./.inngest/main.db`) | PostgreSQL (`--postgres-uri`) |

### Docker Compose Example

```yaml
services:
  inngest:
    image: inngest/inngest
    ports:
      - "8288:8288"
      - "8289:8289"
    command: >
      inngest start
        --event-key ${INNGEST_EVENT_KEY}
        --signing-key ${INNGEST_SIGNING_KEY}
        --redis-uri redis://redis:6379
        --postgres-uri postgresql://user:pass@postgres:5432/inngest
    depends_on:
      redis:
        condition: service_healthy
      postgres:
        condition: service_healthy

  redis:
    image: redis:7-alpine
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]

  postgres:
    image: postgres:16-alpine
    environment:
      POSTGRES_USER: user
      POSTGRES_PASSWORD: pass
      POSTGRES_DB: inngest
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U user"]
```

### Kubernetes/Helm

```bash
helm repo add inngest https://inngest.github.io/helm-charts
helm install inngest inngest/inngest
```

Features: KEDA-based autoscaling, external DB support, non-root execution, read-only filesystem, ingress with SSL/cert-manager.

### Configuration

| Flag | Env Var | Description |
|------|---------|-------------|
| `--port` | `INNGEST_PORT` | Server port (default: 8288) |
| `--signing-key` | `INNGEST_SIGNING_KEY` | Hex string for authentication |
| `--event-key` | `INNGEST_EVENT_KEY` | SDK authentication key |
| `--redis-uri` | `INNGEST_REDIS_URI` | External Redis |
| `--postgres-uri` | `INNGEST_POSTGRES_URI` | External PostgreSQL |
| `--queue-workers` | `INNGEST_QUEUE_WORKERS` | Executor workers (default: 100) |
| `--poll-interval` | `INNGEST_POLL_INTERVAL` | App sync polling (seconds) |

### SDK Configuration for Self-Hosted

```bash
INNGEST_EVENT_KEY=<your-event-key>
INNGEST_SIGNING_KEY=<your-signing-key>
INNGEST_DEV=0
INNGEST_BASE_URL=http://localhost:8288
```

### Limitations vs Cloud

- Single-node only with SQLite (use Postgres for multi-node readiness)
- No guaranteed support (enterprise agreements available)
- No built-in SAML/SSO
- Manual infrastructure management

---

## 13. Local Development

### Dev Server

```bash
npx inngest-cli@latest dev
# Or specify your app URL
npx inngest-cli@latest dev -u http://localhost:3000/api/inngest
```

Access at `http://localhost:8288`.

### Auto-Discovery

The dev server automatically scans common endpoints:
- `/api/inngest`
- `/x/inngest`
- `/.netlify/functions/inngest`

Disable with `--no-discovery`.

### Testing Functions

1. **UI**: Click "Invoke" on any function in the dev server dashboard
2. **SDK**: Send events programmatically via `inngest.send()`
3. **cURL**: `curl -X POST "http://localhost:8288/e/test" -d '{"name":"app/test","data":{}}'`

### Configuration File (inngest.json)

```json
{
  "sdk-url": ["http://localhost:3000/api/inngest"],
  "no-discovery": true
}
```

### Docker

```bash
docker run -p 8288:8288 -p 8289:8289 \
  inngest/inngest inngest dev \
  -u http://host.docker.internal:3000/api/inngest
```

---

## 14. Deployment

### Syncing Apps with Inngest Cloud

After deploying to a hosting platform, sync your app so Inngest knows about your functions:

1. **Manual**: Dashboard > Apps > Sync New App > paste serve endpoint URL
2. **Automated**: Vercel and Netlify integrations sync on every deploy
3. **cURL**: `curl -X PUT https://your-app.com/api/inngest --fail-with-body`

### Required Environment Variables

```bash
INNGEST_SIGNING_KEY=signkey-prod-xxxxx   # Matches Inngest environment
INNGEST_EVENT_KEY=xxxxx                  # For sending events
```

### Resync Triggers

Resync whenever you deploy new function configurations (new functions, changed triggers, updated flow control).

---

## 15. Observability and Monitoring

### Built-In Metrics

| Metric | Description |
|--------|-------------|
| Function Status | Runs grouped by status (failed, succeeded, cancelled) |
| Failed Functions | Top 6 failing functions with frequency |
| Total Runs Throughput | Rate of function runs started |
| Total Steps Throughput | Rate of step executions |
| Backlog | Runs awaiting processing |
| Event Volume | Event throughput and occurrence logs |

### Structured Logging

TypeScript SDK v4 provides Pino-style object-first logging:
- Bring your own logger (Pino, Winston, etc.)
- `internalLogger` option separates SDK logs from application logs

### Traces

Every function run produces structured execution traces showing:
- Each step, its output, and duration
- Where failures occurred
- Prompt/response pairs for AI calls

### Global Search

Command/Ctrl+K to find apps, functions, and events by name or ID.

---

## 16. Security Model

| Feature | Description |
|---------|-------------|
| **Signing Keys** | Pre-shared secrets per environment authenticating SDK-server communication |
| **Replay Prevention** | Timestamps in signatures reject aged requests |
| **Key Rotation** | Without downtime |
| **E2E Encryption** | Middleware encrypts step outputs and function results before leaving your servers |
| **SOC 2 Type II** | Regular independent audits |
| **TLS** | All communication encrypted in transit |
| **SAML/SSO** | Enterprise (free under 200 users) |
| **HIPAA BAA** | Available for enterprise |

### Function Registration Handshake

1. SDK endpoint receives PUT request
2. Configuration sent to Inngest API using signing key as bearer token
3. Apps and functions update idempotently
4. Signing key never exposed to clients

---

## 17. AI and Agent Workflows

### Why Durable Execution Matters for AI

- LLM calls are expensive and slow -- step memoization ensures you pay for each call exactly once
- Multi-step AI pipelines compound failure rates (five 99% steps = 95% success)
- Human-in-the-loop workflows can pause for days via `step.waitForEvent()` without resource consumption
- Automatic retries with configurable backoff handle transient API failures

### Agent Harness Pattern

Inngest serves as an "agent harness" -- the orchestration layer that handles retry, state, routing, concurrency, and observability without being an AI framework itself.

**Agent loop as steps**: Each think-act-observe iteration uses Inngest steps. The LLM call is one step; each tool execution is another.

**Singleton concurrency**: Prevent race conditions with one line:
```typescript
singleton: { key: "event.data.sessionKey", mode: "cancel" }
```

**Sub-agent delegation**: Use `step.invoke()` to spawn focused sub-agents with isolated context windows.

### Three Sub-Agent Patterns

1. **Sync** ("Do this and wait"): Parent blocks until sub-agent returns. Best for data lookups, analysis.
2. **Async** ("Go do this, report back"): Fire-and-forget. Default choice for research, reports.
3. **Scheduled** ("Do this later"): Sub-agent runs at a future time with fresh data.

### User-Defined Workflows

Inngest provides `@inngest/workflow-kit` for building user-configurable workflows:
- Backend workflow engine for composing actions
- Pre-built React components for frontend UI
- Workflow instances stored as JSON in your database
- Action handlers use the same step API

---

## 18. Advanced Patterns

### Email Drip Sequence

```typescript
inngest.createFunction(
  { id: "onboarding-drip", triggers: { event: "app/user.signup" } },
  async ({ event, step }) => {
    await step.run("send-welcome", () => sendEmail("welcome", event.data));
    
    await step.sleep("wait-3d", "3 days");
    
    // Check if user unsubscribed before sending next email
    const unsub = await step.waitForEvent("check-unsub", {
      event: "app/user.unsubscribed",
      timeout: "0s",
      match: "data.userId",
    });
    if (unsub) return;
    
    await step.run("send-tips", () => sendEmail("tips", event.data));
    await step.sleep("wait-7d", "7 days");
    await step.run("send-upgrade", () => sendEmail("upgrade", event.data));
  }
);
```

### Scheduled Fan-Out (Weekly Digest)

```typescript
inngest.createFunction(
  { id: "weekly-digest", triggers: { cron: "TZ=UTC 0 9 * * MON" } },
  async ({ step }) => {
    const users = await step.run("load-users", () => db.users.findActive());
    
    await step.sendEvent("fan-out-digests",
      users.map(u => ({
        name: "app/digest.send",
        data: { userId: u.id },
      }))
    );
  }
);
```

### Human-in-the-Loop Approval

```typescript
inngest.createFunction(
  { id: "expense-approval", triggers: { event: "app/expense.submitted" } },
  async ({ event, step }) => {
    await step.run("notify-manager", () => 
      slack.send(event.data.managerId, `Approve expense #${event.data.id}?`)
    );
    
    const approval = await step.waitForEvent("wait-approval", {
      event: "app/expense.decided",
      match: "data.expenseId",
      timeout: "7d",
    });
    
    if (!approval) {
      await step.run("auto-escalate", () => escalate(event.data));
    } else if (approval.data.approved) {
      await step.run("process-payment", () => pay(event.data));
    } else {
      await step.run("notify-rejection", () => notifyRejection(event.data));
    }
  }
);
```

---

## 19. Inngest Internals

### Queue Architecture

Inngest implements **two queues for every deployed function**. Workers are shared-nothing and pick up work from each queue with minimal contention. The queue system uses:
- Redis for state and queue management
- GCRA (Generic Cell Rate Algorithm) for rate limiting and throttling
- xxhash for collision-free string hashing in event matching

### Event Matching Engine

With thousands of concurrent functions and events, Inngest uses a two-stage matching approach:
1. **Expression normalization**: AST parsing with variable lifting for caching
2. **In-memory indexing**: Hashmaps for string equality, B-trees for numeric comparisons

This collapses evaluation from billions of comparisons to milliseconds.

### Redis Optimization

Inngest cut Redis read operations by 67% with a stateful caching proxy. They shard Redis using run identifiers for even distribution, with a broker pattern that enables zero-downtime migration between clusters.

---

## 20. Comparison with Alternatives

| Feature | Inngest | Temporal | BullMQ | AWS Step Functions |
|---------|---------|----------|--------|--------------------|
| Execution model | Step memoization | Deterministic replay | Job queue | State machine (JSON) |
| Infrastructure | Zero (cloud) or single binary | Temporal server + DB | Redis required | AWS-managed |
| Language support | TS, Python, Go, Kotlin | Go, Java, TS, Python, PHP | Node.js only | Any (via Lambda) |
| Flow control | Declarative (config) | Code-based | Code-based | JSON definition |
| Serverless support | Native | Requires workers | Requires workers | Native |
| Event-driven | Native (first-class events) | Signals (add-on) | Manual | EventBridge integration |
| Learning curve | Low (write normal functions) | High (determinism rules) | Medium | Medium (ASL JSON) |
| Self-hosting | Single binary | Complex (multiple services) | Redis only | Not available |
| Observability | Built-in dashboard | Web UI | Bull Board | CloudWatch |

---

## Common Pitfalls

| Pitfall | Why It Happens | How to Avoid |
|---------|---------------|--------------|
| Non-idempotent steps | Steps may re-execute on retry | Use upserts, idempotency keys |
| Code outside steps | Non-step code runs on every invocation (no memoization) | Wrap all side effects in `step.run()` |
| Forgetting `await` on steps | Sleep/wait won't work without await | Always `await` step calls |
| Large step return values | Step outputs serialized as JSON and stored | Return only essential data (max 4MB for parallel) |
| Changing step IDs | Breaks memoization for in-progress runs | Keep step IDs stable across deploys |
| DST issues with cron | Schedules near transitions execute 0, 1, or 2 times | Use UTC or avoid transition hours |
| Singleton vs concurrency confusion | Singleton limits runs; concurrency limits steps | Choose based on whether you want run-level or step-level control |
| Not setting INNGEST_SIGNING_KEY | App syncs to wrong environment | Always configure per environment |

---

## Best Practices

1. **Wrap all side effects in steps** -- Code outside `step.run()` executes on every invocation and has no retry protection (Source: Inngest docs, execution model)
2. **Keep step return values small** -- Only return data needed by subsequent steps; step outputs are serialized to JSON and stored (Source: Inngest step reference)
3. **Use step IDs descriptively and keep them stable** -- IDs are the memoization keys; changing them breaks in-progress runs (Source: Inngest docs)
4. **Prefer `step.sendEvent()` over `inngest.send()` inside functions** -- Adds tracing context automatically (Source: Inngest sending events guide)
5. **Design for idempotency** -- Use upserts, check-before-write, and idempotency keys on external APIs (Source: Inngest error handling guide)
6. **Use concurrency keys for multi-tenant fairness** -- Prevents any single customer from monopolizing resources (Source: Inngest concurrency guide)
7. **Choose throttle (queue excess) vs rate-limit (drop excess) intentionally** -- Throttle for "process everything eventually"; rate-limit for "reject over threshold" (Source: Inngest flow control docs)
8. **Use fan-out for independent tasks, parallelism for coordinated work** -- Fan-out scales better; parallelism gives you results back (Source: Inngest fan-out guide)
9. **Set `NonRetriableError` for permanent failures** -- Saves time and resources on impossible retries (Source: Inngest retry docs)
10. **Test locally with the dev server before deploying** -- Full feature parity with production at localhost:8288 (Source: Inngest local dev guide)

---

## Further Reading

| Resource | Type | Why Recommended |
|----------|------|-----------------|
| [Inngest Documentation](https://www.inngest.com/docs) | Docs | Official comprehensive reference |
| [How Durable Workflow Engines Work](https://www.inngest.com/blog/how-durable-workflow-engines-work) | Blog | Deep architectural explanation |
| [Principles of Durable Execution](https://www.inngest.com/blog/principles-of-durable-execution) | Blog | Foundational concepts (memoization, checkpointing, state) |
| [Queues Aren't the Right Abstraction](https://www.inngest.com/blog/queues-are-no-longer-the-right-abstraction) | Blog | Why workflow engines > queues |
| [TypeScript SDK v4 Changelog](https://www.inngest.com/blog/typescript-sdk-v4.0) | Blog | v4 features, migration, performance |
| [Self-Hosting Announcement](https://www.inngest.com/blog/inngest-1-0-announcing-self-hosting-support) | Blog | Architecture, deployment, roadmap |
| [GitHub: inngest/inngest](https://github.com/inngest/inngest) | Repo | Server source code (Go) |
| [GitHub: inngest/inngest-js](https://github.com/inngest/inngest-js) | Repo | TypeScript SDK source |
| [GitHub: inngest/inngest-py](https://github.com/inngest/inngest-py) | Repo | Python SDK source |
| [GitHub: inngest/inngestgo](https://github.com/inngest/inngestgo) | Repo | Go SDK source |
| [Durable Execution for AI Agents](https://www.inngest.com/blog/durable-execution-key-to-harnessing-ai-agents) | Blog | AI-specific patterns and durability needs |
| [Your Agent Needs a Harness](https://www.inngest.com/blog/your-agent-needs-a-harness-not-a-framework) | Blog | Agent orchestration architecture |
| [Three Sub-Agent Patterns](https://www.inngest.com/blog/three-patterns-you-need-for-agentic-systems) | Blog | Sync/async/scheduled agent delegation |
| [Redis Sharding at Inngest](https://www.inngest.com/blog/sharding-at-inngest) | Blog | Internal infrastructure scaling |
| [Event Matching Engine](https://www.inngest.com/blog/accidentally-quadratic-evaluating-trillions-of-event-matches-in-real-time) | Blog | Performance optimization deep dive |

---

*This guide was synthesized from 40 sources. See `resources/inngest-sources.json` for full source list with quality scores.*
