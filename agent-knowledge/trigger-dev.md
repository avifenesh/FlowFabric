# Learning Guide: Trigger.dev

**Generated**: 2026-04-09
**Sources**: 40 resources analyzed
**Depth**: deep
**Latest Release**: v4.4.3 (March 2026)
**License**: Apache 2.0

---

## Prerequisites

- TypeScript/JavaScript proficiency (async/await, Promises)
- Familiarity with Node.js or Bun runtimes
- A web application framework (Next.js, Remix, Express, etc.) for triggering tasks
- Basic understanding of background job concepts (queues, retries, scheduling)
- Docker knowledge for self-hosting scenarios

## TL;DR

- Trigger.dev is an open-source background jobs and workflow orchestration platform for TypeScript. You write tasks as plain async functions and deploy them to managed infrastructure with automatic retries, queuing, and observability.
- The v4 SDK uses `task()` to define units of work, `trigger()` / `triggerAndWait()` to execute them, and a checkpoint/restore system (CRIU) that freezes tasks during waits so you pay zero compute while paused.
- Key differentiators: no execution timeouts, durable execution via checkpoint-resume, built-in Realtime API with React hooks, human-in-the-loop waitpoint tokens, and first-class AI agent support with streaming.
- Self-hostable via Docker Compose or Kubernetes Helm charts; commercial cloud available with per-second compute billing starting at $0.0000169/s (micro machine).
- 14,500+ GitHub stars, 615+ releases, Apache 2.0 license.

---

## 1. Architecture and Execution Model

### How It Works

Trigger.dev separates task definition from task execution. You write tasks in your codebase inside a `/trigger` directory, and the platform handles building, deploying, and running them in isolated containers.

**Execution flow:**
1. Your application calls `tasks.trigger()` or `myTask.trigger()` via the SDK
2. Trigger.dev immediately returns a run handle (non-blocking)
3. The task payload enters a queue managed by the platform
4. A worker picks up the task and executes it in an isolated Docker container
5. Results are stored and accessible via the SDK or dashboard

### Checkpoint-Resume System (CRIU)

This is the core architectural innovation. When a task pauses (via `wait.for()`, `triggerAndWait()`, or `wait.forToken()`), the system uses CRIU (Checkpoint/Restore In Userspace) to:

1. Capture the entire process state -- memory, CPU registers, open file descriptors
2. Compress and store the checkpoint to disk
3. Release the compute resources (you stop paying)
4. When the wait condition is met, restore the process to a new container
5. Resume execution from exactly where it left off

This enables virtually unlimited execution time while maintaining serverless economics. Waits longer than ~5 seconds trigger a checkpoint automatically.

### Build System

Tasks are bundled using esbuild during deployment. The same build system runs in both dev and production modes, ensuring parity. The output is packaged into Docker images and deployed to the Trigger.dev instance.

### Observability

The platform uses OpenTelemetry natively. Every task execution generates traces, spans, and logs that auto-correlate parent and child task relationships. Custom instrumentations for HTTP, Prisma, and OpenAI are supported via `trigger.config.ts`.

---

## 2. Task Definition (v4 SDK)

### Basic Task

```typescript
import { task } from "@trigger.dev/sdk";

export const processVideo = task({
  id: "process-video",
  run: async (payload: { videoUrl: string }, { ctx }) => {
    // Long-running work here
    const result = await transcode(payload.videoUrl);
    return { outputUrl: result.url }; // Must be JSON-serializable
  },
});
```

### Configuration Options

```typescript
export const myTask = task({
  id: "my-task",

  // Retry configuration
  retry: {
    maxAttempts: 10,
    factor: 1.8,
    minTimeoutInMs: 500,
    maxTimeoutInMs: 30_000,
    randomize: true,
  },

  // Queue/concurrency
  queue: { concurrencyLimit: 5 },

  // Machine resources
  machine: { preset: "medium-1x" }, // 1 vCPU, 2 GB RAM

  // Maximum execution duration (seconds)
  maxDuration: 3600,

  // Lifecycle hooks
  onStartAttempt: ({ payload, ctx }) => { /* before each attempt */ },
  onSuccess: ({ payload, ctx, output }) => { /* after success */ },
  onFailure: ({ payload, ctx, error }) => { /* after all retries exhausted */ },
  onComplete: ({ payload, ctx }) => { /* regardless of outcome */ },
  onWait: ({ payload, ctx }) => { /* when checkpoint starts */ },
  onResume: ({ payload, ctx }) => { /* when checkpoint restores */ },
  onCancel: ({ payload, ctx }) => { /* when run is cancelled */ },

  // Middleware (wraps entire execution)
  middleware: (payload, { ctx, next }) => {
    // Pre-processing
    return next();
  },

  run: async (payload, { ctx }) => {
    return { result: "done" };
  },
});
```

### Scheduled Tasks (Cron)

```typescript
import { schedules } from "@trigger.dev/sdk";

export const dailyReport = schedules.task({
  id: "daily-report",
  // Declarative schedule -- synced on deploy
  cron: "0 9 * * 1-5", // 9 AM weekdays
  timezone: "America/New_York",
  run: async (payload) => {
    // payload.timestamp - UTC Date of scheduled time
    // payload.lastTimestamp - previous execution time
    // payload.timezone - IANA timezone string
    // payload.scheduleId - schedule identifier
    // payload.externalId - optional external ID (user ID, etc.)
    // payload.upcoming - next 5 scheduled times
    await generateAndSendReport();
  },
});
```

Cron supports standard five-field syntax plus `L` (last day of month) and `1L` (last Monday). Dynamic schedules can be created imperatively via `schedules.create()` for per-user scheduling.

---

## 3. Triggering Tasks

### From Backend Code (Outside Tasks)

```typescript
import { tasks } from "@trigger.dev/sdk";
import type { processVideo } from "~/trigger/video";

// Fire-and-forget
const handle = await tasks.trigger<typeof processVideo>("process-video", {
  videoUrl: "https://example.com/video.mp4",
});

// Batch trigger (up to 1,000 per call)
const batchHandle = await tasks.batchTrigger<typeof processVideo>(
  "process-video",
  videos.map((v) => ({ payload: { videoUrl: v.url } }))
);
```

### From Inside Other Tasks (Task-to-Task)

```typescript
// Fire-and-forget
const handle = await processVideo.trigger({ videoUrl: "..." });

// Wait for result (parent checkpoints, zero compute cost while waiting)
const result = await processVideo.triggerAndWait({ videoUrl: "..." });
if (result.ok) {
  console.log(result.output); // typed output
} else {
  console.error(result.error);
}
// Or use .unwrap() to throw on error:
const output = await processVideo.triggerAndWait({ videoUrl: "..." }).unwrap();

// Batch with wait
const results = await processVideo.batchTriggerAndWait([
  { payload: { videoUrl: "a.mp4" } },
  { payload: { videoUrl: "b.mp4" } },
]);
```

### Trigger Options

| Option | Description | Example |
|--------|-------------|---------|
| `delay` | Schedule for later | `"1h30m"` or `new Date(...)` |
| `ttl` | Auto-expire if not started | `"10m"` |
| `idempotencyKey` | Prevent duplicate runs | `await idempotencyKeys.create("key")` |
| `debounce` | Consolidate rapid triggers | `{ wait: "5s" }` |
| `maxAttempts` | Override retry count | `5` |
| `queue` | Route to specific queue | `"paid-users"` |
| `concurrencyKey` | Per-tenant isolation | `data.userId` |
| `region` | Execution location | `"us-east-1"` |
| `machine` | Compute resources | `{ preset: "large-1x" }` |
| `tags` | Organizational labels | `["user_123", "org_abc"]` |
| `priority` | Queue ordering priority | `1` (higher = sooner) |

### Payload Limits

- Single trigger: up to 3 MB
- Batch trigger: each item up to 3 MB
- Task output: up to 10 MB
- Payloads exceeding 512 KB are automatically offloaded to object storage

---

## 4. Concurrency and Queue Management

### Task-Level Queue

```typescript
export const oneAtATime = task({
  id: "one-at-a-time",
  queue: { concurrencyLimit: 1 },
  run: async (payload) => { /* ... */ },
});
```

### Shared Queues

```typescript
import { queue } from "@trigger.dev/sdk";

const sharedQueue = queue({ name: "shared-processing", concurrencyLimit: 10 });

export const taskA = task({
  id: "task-a",
  queue: sharedQueue,
  run: async (payload) => { /* ... */ },
});

export const taskB = task({
  id: "task-b",
  queue: sharedQueue,
  run: async (payload) => { /* ... */ },
});
```

### Multi-Tenant Isolation with Concurrency Keys

```typescript
// Each userId gets its own concurrency slot within the queue
await generateReport.trigger(data, {
  queue: "free-users",
  concurrencyKey: data.userId,
});
```

### Runtime Queue Management (SDK)

```typescript
import { queues } from "@trigger.dev/sdk";

await queues.list({ page: 1, perPage: 20 });
await queues.pause(queueId);
await queues.resume(queueId);
await queues.overrideConcurrencyLimit(queueId, 50);
await queues.resetConcurrencyLimit(queueId);
```

### Deadlock Prevention

Child tasks do NOT inherit the parent's queue by default. When a parent calls `triggerAndWait()`, it checkpoints and releases its concurrency slot, preventing environment-level deadlocks. Only actively executing runs consume concurrency; waiting runs do not.

### Environment Concurrency

Each environment has a base concurrency limit with a burstable limit (default 2.0x burst factor). Individual queues are capped at the base limit.

---

## 5. Error Handling and Retries

### Retry Configuration

```typescript
// In trigger.config.ts (global defaults)
export default defineConfig({
  retry: {
    maxAttempts: 3,
    factor: 2,
    minTimeoutInMs: 1000,
    maxTimeoutInMs: 60_000,
    randomize: true,
    enabledInDev: false, // retries disabled in dev by default
  },
});
```

### Built-In Retry Helpers

```typescript
import { retry } from "@trigger.dev/sdk";

// Retry a code block
const data = await retry.onThrow(
  async () => {
    return await unstableApi.fetchData();
  },
  { maxAttempts: 5, factor: 1.8 }
);

// Retry HTTP with status-aware backoff
const response = await retry.fetch("https://api.example.com/data", {
  retry: {
    byStatus: {
      "429": { strategy: "headers", header: "Retry-After" },
      "500-599": { strategy: "backoff", maxAttempts: 3 },
    },
  },
});
```

### Preventing Retries

```typescript
import { AbortTaskRunError } from "@trigger.dev/sdk";

// Immediately fail without retrying
throw new AbortTaskRunError("Invalid input - do not retry");
```

### Conditional Error Handling

```typescript
export const myTask = task({
  id: "my-task",
  catchError: ({ error, retryAt, retryDelayInMs }) => {
    if (error.message.includes("rate limit")) {
      return { retryAt: new Date(Date.now() + 60_000) };
    }
    if (error.message.includes("invalid input")) {
      return { skipRetrying: true };
    }
    // Default retry behavior
  },
  run: async (payload) => { /* ... */ },
});
```

---

## 6. Waits and Waitpoint Tokens

### Time-Based Waits

```typescript
import { wait } from "@trigger.dev/sdk";

// Wait for a duration (task checkpoints, zero compute cost)
await wait.for({ seconds: 30 });
await wait.for({ hours: 1, minutes: 30 });

// Wait until a specific date
await wait.until({ date: new Date("2026-12-25T00:00:00Z") });
```

### Waitpoint Tokens (Human-in-the-Loop)

```typescript
import { wait } from "@trigger.dev/sdk";

// Create a token for external completion
const token = await wait.createToken({
  timeout: "24h",
  tags: ["approval"],
});
// token.id - waitpoint ID (prefixed with waitpoint_)
// token.url - HTTP callback URL for external services
// token.publicAccessToken - for client-side completion

// Wait for the token to be completed
const result = await wait.forToken<{ status: string }>(token.id);
if (result.ok) {
  console.log("Approved:", result.output.status);
} else {
  console.log("Timed out");
}
```

**Completing tokens externally:**

```typescript
// From SDK
await wait.completeToken(tokenId, { status: "approved" });

// Via HTTP (from any language)
// POST /api/v1/waitpoints/tokens/{tokenId}/complete
// Authorization: Bearer <secret-key>
// Body: { "output": { "status": "approved" } }
```

### Input Streams

```typescript
import { inputStream } from "@trigger.dev/sdk";

// Wait for data to arrive on a named stream
const data = await inputStream.wait("user-feedback");
```

---

## 7. Realtime API

### Architecture

The Realtime system uses Electric SQL (HTTP-based PostgreSQL sync) for run updates and a dedicated transport for streaming. No WebSockets required.

### Backend Subscriptions

```typescript
import { runs } from "@trigger.dev/sdk";

// Subscribe to a specific run
for await (const run of runs.subscribeToRun(runId)) {
  console.log("Status:", run.status);
  if (run.status === "COMPLETED") break;
}

// Subscribe with streams
const subscription = runs.subscribeToRun(runId).withStreams();
```

### React Hooks

```typescript
import { useRealtimeRun } from "@trigger.dev/react-hooks";

function RunStatus({ runId, accessToken }) {
  const { run, error } = useRealtimeRun(runId, {
    accessToken, // Public Access Token required
  });

  if (error) return <div>Error: {error.message}</div>;
  if (!run) return <div>Loading...</div>;

  return (
    <div>
      <p>Status: {run.status}</p>
      <p>Output: {JSON.stringify(run.output)}</p>
    </div>
  );
}
```

### Streaming (AI/LLM Tokens)

Define streams in task code and consume them in real-time on the frontend:

```typescript
// Task definition
import { task, streams } from "@trigger.dev/sdk";

const aiStream = streams.define("ai-output");

export const chatTask = task({
  id: "chat",
  run: async (payload: { prompt: string }) => {
    const response = await openai.chat.completions.create({
      model: "gpt-4",
      messages: [{ role: "user", content: payload.prompt }],
      stream: true,
    });
    // Pipe AI tokens to the stream
    for await (const chunk of response) {
      await aiStream.write(chunk.choices[0]?.delta?.content ?? "");
    }
    return { done: true };
  },
});
```

Backend stream consumption:

```typescript
const stream = await streams.read<string>(runId, "ai-output");
for await (const chunk of stream) {
  process.stdout.write(chunk);
}
```

---

## 8. Idempotency

### Scopes

| Scope | Behavior | Use Case |
|-------|----------|----------|
| `"run"` (default) | Unique per parent run | Prevent duplicate children during retries |
| `"attempt"` | Unique per attempt | Re-execute on each retry |
| `"global"` | Single execution everywhere | Absolute deduplication |

```typescript
import { idempotencyKeys } from "@trigger.dev/sdk";

// Create scoped key
const key = await idempotencyKeys.create("send-email-user-123");
await sendEmail.trigger(payload, {
  idempotencyKey: key,
  idempotencyKeyTTL: "1h", // Expire after 1 hour (default: 30 days)
});

// Array-based keys
const key2 = await idempotencyKeys.create([userId, "onboarding"]);

// Reset a key to allow re-execution
await idempotencyKeys.reset("send-email", "send-email-user-123");
```

Failed runs automatically clear their idempotency keys. Successful and cancelled runs retain them.

---

## 9. Run Lifecycle

### States

| Phase | Status | Description |
|-------|--------|-------------|
| Initial | `PENDING_VERSION` | Awaiting code version deployment |
| Initial | `DELAYED` | Waiting for delay period |
| Initial | `QUEUED` | Ready, waiting in queue |
| Initial | `DEQUEUED` | Sent to worker |
| Active | `EXECUTING` | Running on a worker |
| Active | `WAITING` | Paused at wait/triggerAndWait (checkpointed) |
| Final | `COMPLETED` | Successful |
| Final | `CANCELED` | Manually stopped |
| Final | `FAILED` | Error after exhausting retries |
| Final | `TIMED_OUT` | Exceeded maxDuration |
| Final | `CRASHED` | Worker process failure (no retry) |
| Final | `SYSTEM_FAILURE` | Unrecoverable system error |
| Final | `EXPIRED` | TTL passed before execution |

### Management API

```typescript
import { runs } from "@trigger.dev/sdk";

// List with filters
for await (const run of runs.list({
  status: ["COMPLETED", "FAILED"],
  tag: "user_123",
})) {
  console.log(run.id, run.status);
}

// Single run
const run = await runs.retrieve(runId);

// Actions
await runs.cancel(runId);
await runs.replay(runId); // New run with same payload
await runs.reschedule(runId, { delay: "1h" });
```

### Tags

- Up to 10 tags per run (combined from trigger-time and runtime)
- Recommended format: `type_value` or `type:value` (e.g., `user_123`, `org:abc`)
- Tags do not propagate to child runs automatically

```typescript
import { tags } from "@trigger.dev/sdk";

// Add at trigger time
await myTask.trigger(payload, { tags: ["user_123"] });

// Add during execution
await tags.add("processed_video_456");
```

---

## 10. Configuration (trigger.config.ts)

```typescript
import { defineConfig } from "@trigger.dev/sdk/config";
import { prismaExtension } from "@trigger.dev/build/extensions/prisma";
import { syncEnvVars } from "@trigger.dev/build/extensions";

export default defineConfig({
  project: "proj_xxxxxxxxxxxx",

  // Task directories (auto-detects "trigger" folders if omitted)
  dirs: ["./trigger"],

  // Runtime: "node" (21.7.3), "node-22" (22.16.0), or "bun" (1.3.3)
  runtime: "node",

  // Default machine for all tasks
  defaultMachine: "small-1x",

  // Default max duration (seconds)
  maxDuration: 300,

  // Global retry defaults
  retry: {
    maxAttempts: 3,
    factor: 2,
    minTimeoutInMs: 1000,
    maxTimeoutInMs: 30_000,
    randomize: true,
    enabledInDev: false,
  },

  // Log level for logger API
  logLevel: "info",

  // Reuse processes between task executions
  processKeepAlive: true,

  // Global lifecycle hooks
  onStart: ({ payload, ctx }) => {},
  onSuccess: ({ payload, ctx, output }) => {},
  onFailure: ({ payload, ctx, error }) => {},
  init: () => {},

  // Telemetry
  telemetry: {
    instrumentations: [
      // new PrismaInstrumentation(),
      // new OpenAIInstrumentation(),
    ],
    exporters: [
      // Custom OTLP exporters for Axiom, Honeycomb, etc.
    ],
  },

  // Build configuration
  build: {
    external: ["sharp"], // Packages excluded from bundling
    autoDetectExternal: true,
    keepNames: true,
    minify: false,
    extensions: [
      prismaExtension({ version: "6.x", schema: "./prisma/schema.prisma" }),
      syncEnvVars(),
      // aptGet({ packages: ["ffmpeg"] }),
      // puppeteer(),
      // ffmpeg(),
      // python({ requirements: "./requirements.txt" }),
    ],
  },
});
```

---

## 11. Machine Presets and Pricing

### Machine Types

| Preset | vCPU | RAM | Disk | Cost/Second |
|--------|------|-----|------|-------------|
| micro | 0.25 | 0.25 GB | 10 GB | $0.0000169 |
| small-1x (default) | 0.5 | 0.5 GB | 10 GB | $0.0000338 |
| small-2x | 1 | 1 GB | 10 GB | $0.0000675 |
| medium-1x | 1 | 2 GB | 10 GB | $0.0000850 |
| medium-2x | 2 | 4 GB | 10 GB | $0.0001700 |
| large-1x | 4 | 8 GB | 10 GB | $0.0003400 |
| large-2x | 8 | 16 GB | 10 GB | $0.0006800 |

### Run Invocation Fee

$0.000025 per run ($0.25 per 10,000 runs). Development environment runs are free.

### Plan Tiers

| Feature | Free | Hobby ($10/mo) | Pro ($50/mo) | Enterprise |
|---------|------|-----------------|--------------|------------|
| Monthly credits | $5 | $10 | $50 | Custom |
| Concurrent runs | 20 | 50 | 200+ | Custom |
| Schedules | 10 | 100 | 1,000+ | Custom |
| Log retention | 1 day | 7 days | 30 days | Custom |
| Preview branches | 0 | 5 | 20+ | Custom |
| Realtime connections | 10 | 50 | 500+ | Custom |
| Team members | 5 | 5 | 25+ | Custom |
| Support | Community | Community | Dedicated Slack | Priority |

Additional Pro add-ons: +50 concurrent runs ($10/mo), +1,000 schedules ($10/mo), +1 team seat ($20/mo).

### Cost Optimization

- Waits do NOT count toward compute cost -- checkpoint-resume means zero billing during pauses
- Time between retry attempts is not billed
- Use the smallest machine that fits your workload
- Use `usage.measure()` to identify expensive code blocks
- Batch triggers are more efficient than individual trigger loops
- Dev environment runs are not charged invocation fees

---

## 12. Deployment

### CLI Deploy

```bash
# Production (default)
npx trigger.dev@latest deploy

# Staging
npx trigger.dev@latest deploy --env staging

# Preview branch
npx trigger.dev@latest deploy --env preview --branch feature-x

# Dry run (build without deploying)
npx trigger.dev@latest deploy --dry-run

# Skip version promotion
npx trigger.dev@latest deploy --skip-promotion
```

### GitHub Actions

```yaml
name: Deploy Trigger.dev (Production)
on:
  push:
    branches: [main]
    paths: ["trigger/**"]

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: "20"
      - run: npm ci
      - run: npx trigger.dev@latest deploy
        env:
          TRIGGER_ACCESS_TOKEN: ${{ secrets.TRIGGER_ACCESS_TOKEN }}
```

Preview branches: listen for `pull_request` events with types `[opened, synchronize, reopened, closed]`. The `closed` event triggers archive cleanup.

### Vercel Integration

The Vercel integration provides:
- Automatic deployment on every Vercel push
- Environment variable sync (Vercel to/from Trigger.dev)
- Atomic deployments: gates Vercel deployment until task build completes, then redeploys with correct `TRIGGER_VERSION`
- Preview environment mapping

### Version Management

Versions follow `YYYYMMDD.N` format (e.g., `20260409.1`). When a task starts executing, it locks to the current version. Child tasks called with `triggerAndWait()` inherit the parent's version; those called with `trigger()` use the latest version.

**Important:** Keep CLI and `@trigger.dev/*` package versions in sync. Add `trigger.dev` to `devDependencies` and use npm scripts rather than `@latest`.

---

## 13. Self-Hosting

### Requirements

| Requirement | Minimum |
|-------------|---------|
| CPU | 4 cores |
| RAM | 8 GB |
| OS | Debian or Debian-derivative |
| Software | Docker, Docker Compose |
| Network | Ngrok (for internet exposure) |

### Docker Compose Deployment

Two deployment patterns:

1. **Single Server**: All components (webapp + workers) on one machine via `./start.sh`
2. **Split Services**: Webapp and workers on separate machines via `./start.sh webapp` and `./start.sh worker`

### Critical Environment Variables

- `MAGIC_LINK_SECRET`, `SESSION_SECRET`, `ENCRYPTION_KEY` -- Authentication secrets
- `PROVIDER_SECRET`, `COORDINATOR_SECRET` -- Internal service communication
- `TRIGGER_PROTOCOL`, `TRIGGER_DOMAIN` -- Internet exposure
- `DEPLOY_REGISTRY_HOST`, `DEPLOY_REGISTRY_NAMESPACE` -- Docker registry for task images

### Kubernetes (Helm Charts)

Official Helm charts are available for Kubernetes deployments, documented separately from the Docker Compose path.

### Current Limitations

- No ARM architecture support for workers
- Docker checkpoint (CRIU) is experimental
- No built-in resource limits on task containers
- Host networking exposes task containers to networked services
- Multi-worker machine scaling is not yet supported
- The self-hosted setup is described as "not production-ready" for security/scaling

### Authentication Options

- Magic link email (default) -- requires email transport (Resend, SMTP, or AWS SES)
- GitHub OAuth (not recommended for self-hosted due to security concerns)
- Email allowlisting via `WHITELISTED_EMAILS` regex

---

## 14. AI Agent Patterns

Trigger.dev supports five core agent patterns (inspired by Anthropic's research):

### 1. Prompt Chaining
Sequential prompts where each step feeds into the next. Example: generate marketing copy, then translate it.

### 2. Routing
Direct queries to different models based on complexity or type.

### 3. Parallelization
Run multiple AI tasks simultaneously (e.g., content moderation + response generation).

### 4. Orchestrator
Coordinate multiple AI workers for complex verification (e.g., fact-checking news articles).

### 5. Evaluator-Optimizer
Implement feedback loops where one model evaluates another's output and requests improvements.

### AI Framework Integrations

- Claude / Anthropic SDK
- OpenAI Agent SDK (both Python and TypeScript)
- Vercel AI SDK
- Mastra (multi-agent with memory)

### Streaming for AI

Realtime streams enable token-by-token display of LLM responses. Define typed streams in tasks, pipe AI output, and consume in React via `useRealtimeRunWithStreams`.

---

## 15. Logging, Tracing, and Observability

### Structured Logging

```typescript
import { logger } from "@trigger.dev/sdk";

logger.debug("Detailed info", { key: "value" });
logger.info("Processing started", { userId: "123" });
logger.warn("Approaching limit", { usage: 95 });
logger.error("Operation failed", { error: err.message });
```

### Custom Traces

```typescript
const result = await logger.trace("db-operation", async (span) => {
  span.setAttribute("table", "users");
  return await db.query("SELECT * FROM users");
});
```

### Usage Tracking

```typescript
import { usage } from "@trigger.dev/sdk";

// Current run costs
const current = usage.getCurrent();
console.log("Cost so far:", current.totalCostInCents);

// Measure a specific block
const { result, compute } = await usage.measure(async () => {
  return await expensiveOperation();
});
console.log("Block cost:", compute.costInCents, "Duration:", compute.durationMs);
```

### External Telemetry Exporters

Configure OTLP exporters in `trigger.config.ts` for Axiom, Honeycomb, or any OpenTelemetry-compatible backend.

---

## 16. Build Extensions

Build extensions customize the Docker image built for your tasks.

| Extension | Purpose |
|-----------|---------|
| `prismaExtension` | Include Prisma ORM and generate client |
| `syncEnvVars` | Sync env vars from Infisical/Vercel at deploy time |
| `puppeteer` | Headless Chrome/Chromium |
| `playwright` | Browser automation framework |
| `ffmpeg` | Video/audio processing |
| `python` | Python runtime with pip requirements |
| `aptGet` | Install system packages via apt-get |
| `additionalFiles` | Copy extra files into build |
| `additionalPackages` | Include npm packages not in bundle |
| `esbuildPlugin` | Custom esbuild plugins |
| `emitDecoratorMetadata` | TypeScript decorator support |
| `lightpanda` | Lightweight browser automation |
| `audioWaveform` | Audio processing support |

---

## 17. Framework Integration Patterns

### Next.js (App Router)

```typescript
// app/api/process/route.ts
import { tasks } from "@trigger.dev/sdk";
import type { processVideo } from "~/trigger/video";

export async function POST(request: Request) {
  const data = await request.json();
  const handle = await tasks.trigger<typeof processVideo>("process-video", data);
  return Response.json({ runId: handle.id });
}
```

### Next.js (Server Actions)

```typescript
"use server";
import { tasks } from "@trigger.dev/sdk";
import type { processVideo } from "~/trigger/video";

export async function startProcessing(videoUrl: string) {
  return await tasks.trigger<typeof processVideo>("process-video", { videoUrl });
}
```

### Local Development

```bash
# Run Next.js and Trigger.dev dev server concurrently
npx concurrently --raw --kill-others "next dev" "npx trigger.dev@latest dev"
```

### Supported Frameworks

Next.js, Remix, SvelteKit, Node.js (Express/Fastify), Bun, and any framework that can make HTTP calls.

---

## 18. System Limits

| Limit | Free | Hobby | Pro |
|-------|------|-------|-----|
| API rate limit | 1,500 req/min | 1,500 req/min | 1,500 req/min |
| Concurrent runs | 10 | 25 | 100+ |
| Queued tasks (prod) | 10,000 | 250,000 | 1,000,000 |
| Queued tasks (dev) | 500 | 500 | 5,000 |
| Schedules | 10 | 100 | 1,000+ |
| Projects per org | 10 | 10 | 10 |
| Max run TTL | 14 days | 14 days | 14 days |
| Payload size | 3 MB | 3 MB | 3 MB |
| Output size | 10 MB | 10 MB | 10 MB |
| Tags per run | 10 | 10 | 10 |
| Batch size | 1,000 | 1,000 | 1,000 |

---

## 19. Migration: v3 to v4

### Critical Timeline
- **April 1, 2026**: New v3 deploys stop working
- **July 1, 2026**: v3 fully shut down

### Key Breaking Changes

1. **Import path**: `@trigger.dev/sdk/v3` becomes `@trigger.dev/sdk`
2. **Queues**: Must be predefined with `queue()` instead of created on-demand
3. **Lifecycle hooks**: Unified object parameter signatures `({ payload, ctx, output })` instead of positional args
4. **triggerAndWait**: Returns Result objects (`result.ok` / `result.error`) instead of direct values
5. **batchTrigger**: Use `await batch.retrieve(batchHandle.batchId)` instead of `batchHandle.runs`
6. **Context**: Removed `ctx.attempt.id`, `ctx.attempt.status`, `ctx.task.exportName`
7. **handleError** renamed to **catchError**
8. **init** replaced by middleware and `locals` API

### New v4 Features
- Waitpoint tokens (human-in-the-loop)
- Priority system for queue ordering
- Global lifecycle hooks
- `onWait`, `onResume`, `onComplete`, `onCancel` callbacks
- Hidden tasks (non-exported but executable)
- `useWaitToken` React hook
- AI tool generation from schema tasks
- Node.js 22 and Bun runtime options

---

## 20. Testing

### Dashboard Test Runner

The Trigger.dev dashboard provides a built-in test interface:
1. Select a task from the side menu
2. Configure payload and metadata
3. Set advanced options (machine, queue, delay)
4. Reference previous test runs
5. Save reusable test templates
6. Execute and monitor results

### Local Development

The `npx trigger.dev@latest dev` command runs tasks locally with hot reloading. Each task runs in a separate Node process. Use `--analyze` to debug slow builds.

---

## Common Pitfalls

| Pitfall | Why It Happens | How to Avoid |
|---------|----------------|--------------|
| Version mismatch errors | CLI and SDK package versions out of sync | Add `trigger.dev` to devDependencies; use npm scripts instead of `@latest` |
| Deadlocks in queues | Parent and child on same queue with concurrency=1 | Use separate queues for parent/child; children don't inherit parent queues |
| Duplicate task executions | Missing idempotency keys during retries | Use `idempotencyKeys.create()` for child task triggers |
| Unexpected costs on waits | Assuming waits cost compute | They don't -- checkpoint-resume means zero cost during waits |
| Payload too large errors | Exceeding 3 MB limit | Use object storage URLs in payloads instead of raw data |
| Tags not on child runs | Assuming tag propagation | Explicitly pass tags via context or trigger options |
| OOM crashes | Machine too small for workload | Use `ResourceMonitor` to track memory; configure auto-retry with larger machine |
| Bun OTEL issues | Bun doesn't support Node register hooks | Accept OTEL instrumentation limitations with Bun runtime |
| Self-hosted ARM failure | Workers don't support ARM | Use x86_64 architecture for worker nodes |

---

## Best Practices

1. **Decompose complex workflows into subtasks** -- Each subtask gets independent retries and observability (Source: Trigger.dev Docs - Errors & Retrying)
2. **Use idempotency keys for all child task triggers** -- Prevents duplicate work during parent retries (Source: Trigger.dev Docs - Idempotency)
3. **Set appropriate machine presets per task** -- Don't over-provision; use `usage.measure()` to profile (Source: Trigger.dev Docs - Machines)
4. **Leverage checkpoint-resume for cost savings** -- Use `wait.for()` and `triggerAndWait()` instead of polling loops (Source: Trigger.dev Docs - How It Works)
5. **Use tags for multi-tenant filtering** -- Prefix with type (`user_123`, `org_abc`) for dashboard filtering (Source: Trigger.dev Docs - Tags)
6. **Keep CLI and SDK versions synchronized** -- Pin versions in package.json (Source: Trigger.dev Docs - GitHub Actions)
7. **Use `catchError` for conditional retry logic** -- Skip retries for known-bad inputs, custom delay for rate limits (Source: Trigger.dev Docs - Errors & Retrying)
8. **Use shared queues for related tasks** -- Better concurrency control across task types (Source: Trigger.dev Docs - Queue Concurrency)
9. **Set TTL on runs to prevent queue bloat** -- Expire runs that wait too long (Source: Trigger.dev Docs - Runs)
10. **Use atomic deploys with Vercel** -- Gates deployment until task build completes (Source: Trigger.dev Docs - Vercel Integration)

---

## Further Reading

| Resource | Type | Why Recommended |
|----------|------|-----------------|
| [Official Documentation](https://trigger.dev/docs) | Docs | Comprehensive reference for all features |
| [GitHub Repository](https://github.com/triggerdotdev/trigger.dev) | Source | Apache 2.0 source, 14.5k stars, 615+ releases |
| [Quick Start Guide](https://trigger.dev/docs/quick-start) | Tutorial | 3-minute setup walkthrough |
| [AI Agent Patterns](https://trigger.dev/docs/guides/ai-agents) | Guide | Five agent architecture patterns with examples |
| [Realtime API](https://trigger.dev/docs/realtime/overview) | Docs | Electric SQL-based live updates and streaming |
| [Self-Hosting Guide](https://trigger.dev/docs/open-source-self-hosting) | Docs | Docker Compose and Kubernetes deployment |
| [v3-to-v4 Migration](https://trigger.dev/docs/migrating-from-v3) | Guide | Breaking changes and migration steps |
| [Pricing Calculator](https://trigger.dev/pricing) | Reference | Per-second compute costs by machine type |
| [Example Projects](https://trigger.dev/docs/examples/overview) | Examples | 30+ practical implementations |
| [Discord Community](https://trigger.dev/discord) | Community | 4,700+ members, self-hosting channel |
| [Blog: Bun 5x Throughput](https://trigger.dev/blog) | Blog | Infrastructure decisions and performance benchmarks |
| [LLM Documentation Index](https://trigger.dev/docs/llms.txt) | Index | Complete URL listing of all documentation pages |
| [Management API](https://trigger.dev/docs/management/overview) | API | REST API for runs, schedules, env vars, and queues |
| [Vercel Integration](https://trigger.dev/docs/vercel-integration) | Guide | Atomic deploys and env var sync |
| [TRQL Query Language](https://trigger.dev/docs/query) | Docs | SQL-style querying on ClickHouse-backed run data |

---

*This guide was synthesized from 40 sources. See `resources/trigger-dev-sources.json` for full source list with quality scores.*
