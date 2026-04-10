# Learning Guide: glide-mq

**Generated**: 2026-04-09
**Sources**: 22 resources analyzed
**Depth**: medium

## Prerequisites

- Solid understanding of Node.js (async/await, EventEmitter, streams)
- Familiarity with message queue concepts (producers, consumers, jobs, workers)
- Basic knowledge of Valkey or Redis (data structures, commands, persistence)
- Node.js 20+ runtime environment
- Valkey 7.0+ or Redis 7.0+ server (or use in-memory TestQueue for development)

## TL;DR

- glide-mq is a high-performance Node.js message queue built on Valkey/Redis Streams with a Rust NAPI core, achieving 35-38% higher throughput than alternatives via a single-round-trip-per-job architecture.
- It ships seven AI-native primitives (cost tracking, token streaming, suspend/resume, fallback chains, TPM rate limiting, budget caps, adaptive lock duration) for LLM pipeline orchestration.
- The API is deliberately BullMQ-compatible (Queue, Worker, FlowProducer) with a documented migration path, while making previously Pro-only features (group concurrency, ordering) free and open source under Apache 2.0.
- Internally it uses persistent Valkey Functions (FUNCTION LOAD + FCALL) instead of ephemeral Lua EVAL scripts, with hash-tagged keys for zero-configuration clustering.
- Created by Avi Fenesh, a System Software Engineer at AWS ElastiCache who also maintains the Valkey GLIDE client.

---

## Core Concepts

### What Is glide-mq?

glide-mq is a task queue and job scheduling library for Node.js that uses Valkey (or Redis) as its backing store. It is not a log-based event streaming platform like Kafka -- it is a work queue where jobs are submitted by producers, stored in Valkey, and processed by workers with at-least-once delivery semantics.

The library distinguishes itself through two architectural decisions:

1. **Rust NAPI client layer** -- The underlying Valkey connection uses `@glidemq/speedkey`, a binding built on the Valkey GLIDE Rust core. This eliminates JavaScript-level protocol parsing overhead that libraries like ioredis incur.

2. **1-RTT-per-job optimization** -- When a worker completes a job and needs the next one, glide-mq does both operations in a single server-side FCALL. BullMQ requires 2-3 separate round trips. This compounds under real network latency, yielding 35-38% throughput gains in cloud benchmarks.

### Architecture: How It Uses Valkey

All queue state lives in native Valkey data structures under a consistent hash-tagged key scheme:

| Data Structure | Key Pattern | Purpose |
|---|---|---|
| Stream | `glide:{queueName}:stream` | Ready jobs (FIFO consumption via XREADGROUP) |
| Stream | `glide:{queueName}:events` | Lifecycle events for monitoring |
| Sorted Set | `glide:{queueName}:scheduled` | Delayed and prioritized jobs |
| Sorted Set | `glide:{queueName}:completed` | Completed job archive |
| Sorted Set | `glide:{queueName}:failed` | Failed job archive |
| Hash | `glide:{queueName}:job:{id}` | Job metadata (30+ fields) |
| Hash | `glide:{queueName}:meta` | Queue configuration |
| List | `glide:{queueName}:lifo` | LIFO consumption via RPUSH/RPOP |
| Set | `glide:{queueName}:deps:{id}` | Flow dependency tracking |

The `{queueName}` hash tag ensures all keys for a queue land on the same cluster slot, enabling atomic multi-key operations without cross-slot errors.

### Valkey Functions vs. Lua EVAL Scripts

BullMQ ships 53 separate Lua scripts that are loaded via EVAL on each call. glide-mq consolidates all server-side logic into a single persistent function library (`glidemq`) with 44 named functions:

| Aspect | BullMQ (EVAL) | glide-mq (FUNCTION LOAD) |
|---|---|---|
| Loading | Per-call, ephemeral | Loaded once, persists in RDB/AOF |
| Restarts | Script cache lost | Survives server restarts |
| Updates | Re-evaluate on each call | Atomic library replacement |
| Naming | SHA1 digests | Named functions (glidemq_addJob, etc.) |
| Composition | Independent scripts | Functions can call each other |

On initialization, glide-mq checks if the library exists via `FUNCTION LIST`, loads it via `FUNCTION LOAD` if missing, and uses `FUNCTION LOAD REPLACE` for version updates. In cluster mode, this broadcasts to all primaries.

### Job State Machine

Jobs flow through well-defined states mapped to Valkey structures:

```
waiting (in stream, not claimed) 
  --> active (in PEL via XREADGROUP)
    --> completed (ZSet) | failed (ZSet)

delayed (scheduled ZSet with future timestamp)
  --> waiting (promoted by scheduler)

prioritized (scheduled ZSet with encoded score)
  --> waiting (promoted immediately)

waiting-children (parent waiting for deps)
  --> active (all children completed)
```

Priority encoding uses `score = priority * 2^42 + timestamp_ms`, ensuring priority ordering with FIFO tiebreaking.

### Consumer Group Strategy

Each queue has one consumer group (`glide:{queueName}:workers`). Each Worker instance registers as a consumer (`worker-{uuid}`) and reads via `XREADGROUP GROUP workers worker-{uuid} COUNT {prefetch} BLOCK {timeout}`. Jobs are acknowledged via XACK after completion or failure. Stalled job recovery uses XAUTOCLAIM rather than lock-based polling.

---

## API Reference

### Queue

The primary producer class. Creates jobs and manages queue state.

```typescript
import { Queue } from 'glide-mq';

const queue = new Queue('tasks', {
  connection: { addresses: [{ host: 'localhost', port: 6379 }] }
});

// Add a single job
const job = await queue.add('send-email', { to: 'user@example.com' }, {
  delay: 5000,        // delay 5 seconds
  priority: 1,        // higher priority
  attempts: 3,        // retry up to 3 times
  backoff: { type: 'exponential', delay: 1000 },
  removeOnComplete: true,
  ttl: 30000,         // expire after 30s if not processed
});

// Bulk add (12.7x faster than serial via GLIDE Batch API)
await queue.addBulk([
  { name: 'task-a', data: { x: 1 } },
  { name: 'task-b', data: { x: 2 } },
]);

// Request-reply (RPC pattern)
const result = await queue.addAndWait('compute', { input: 42 });

// Queue management
await queue.pause();
await queue.resume();
await queue.drain();                    // remove waiting/delayed jobs
await queue.clean(3600000, 100, 'completed');  // clean old completed
await queue.obliterate({ force: true }); // wipe everything
```

**Key Queue methods**: `add()`, `addBulk()`, `addAndWait()`, `getJob()`, `getJobs()`, `getJobCounts()`, `getMetrics()`, `getJobLogs()`, `getFlowUsage()`, `getUsageSummary()`, `readStream()`, `pause()`, `resume()`, `drain()`, `clean()`, `obliterate()`, `close()`.

### Worker

Processes jobs from a queue with configurable concurrency and resilience.

```typescript
import { Worker } from 'glide-mq';

const worker = new Worker('tasks', async (job) => {
  console.log(`Processing ${job.name} [${job.id}]`);
  
  await job.log('Starting processing');
  await job.updateProgress(50);
  
  // Business logic here
  const result = await processTask(job.data);
  
  await job.updateProgress(100);
  return result;  // stored as job.returnvalue
}, {
  connection: { addresses: [{ host: 'localhost', port: 6379 }] },
  concurrency: 10,
  lockDuration: 30000,
  stalledInterval: 30000,
  limiter: { max: 100, duration: 60000 },  // rate limit
});

// Event handling
worker.on('completed', (job, result) => { /* ... */ });
worker.on('failed', (job, err) => { /* ... */ });
worker.on('stalled', (jobId) => { /* ... */ });
worker.on('error', (err) => { /* ... */ });
worker.on('drained', () => { /* ... */ });
```

**Worker events**: `active`, `completed`, `failed`, `error`, `stalled`, `drained`, `closing`.

**Error handling patterns**:
- Imperative: `job.discard(); throw new Error('permanent failure');`
- Declarative: `throw new UnrecoverableError('permanent failure');`

### Producer (Serverless)

Lightweight job producer for Lambda/Edge -- returns string IDs with minimal overhead.

```typescript
import { serverlessPool } from 'glide-mq';

export async function handler(event) {
  const producer = serverlessPool.getProducer('notifications', {
    connection: { addresses: [{ host: process.env.VALKEY_HOST, port: 6379 }] },
  });
  
  const id = await producer.add('push', { userId: event.userId });
  return { statusCode: 200, body: JSON.stringify({ jobId: id }) };
}

process.on('SIGTERM', () => serverlessPool.closeAll());
```

Cold starts create a new connection; warm invocations reuse the cached producer. Container freeze/thaw auto-reconnects.

### FlowProducer (Workflows)

Orchestrates job trees, DAGs, chains, and groups.

```typescript
import { FlowProducer } from 'glide-mq';

const flow = new FlowProducer({ connection });

// Parent-child tree (parent runs after all children complete)
await flow.add({
  name: 'aggregate',
  queueName: 'reports',
  data: {},
  children: [
    { name: 'fetch-sales', queueName: 'data', data: { source: 'sales' } },
    { name: 'fetch-inventory', queueName: 'data', data: { source: 'inv' } },
  ],
});

// DAG with fan-in
await flow.addDAG({
  nodes: [
    { name: 'extract', queueName: 'etl', data: {} },
    { name: 'transform', queueName: 'etl', data: {}, deps: ['extract'] },
    { name: 'validate', queueName: 'etl', data: {}, deps: ['extract'] },
    { name: 'load', queueName: 'etl', data: {}, deps: ['transform', 'validate'] },
  ],
});
```

**Helpers**: `chain()` (sequential), `group()` (parallel), `chord()` (parallel + callback).

**Dynamic children**: When child count is unknown at submission, a parent processor can create children on the fly, then call `job.moveToWaitingChildren()` to pause until they complete.

### Scheduler

Cron and interval-based job scheduling.

```typescript
import { Scheduler } from 'glide-mq';

const scheduler = new Scheduler('reports', { connection });

// Cron (5-field with timezone)
await scheduler.upsertJobScheduler('daily-report', {
  pattern: '0 9 * * *',
  tz: 'America/New_York',
}, { name: 'generate-report', data: { type: 'daily' } });

// Fixed interval
await scheduler.upsertJobScheduler('health-check', {
  every: 30000,  // every 30 seconds
}, { name: 'ping', data: {} });

// Bounded (startDate, endDate, limit)
await scheduler.upsertJobScheduler('promo-campaign', {
  pattern: '0 12 * * *',
  startDate: new Date('2026-05-01'),
  endDate: new Date('2026-05-31'),
}, { name: 'send-promo', data: {} });
```

Schedulers persist in Valkey and survive worker restarts.

### Broadcast (Pub/Sub)

Fan-out messaging with NATS-style subject filtering.

```typescript
import { Broadcast, BroadcastWorker } from 'glide-mq';

const broadcast = new Broadcast('events', { connection });
await broadcast.publish('orders.created', { orderId: 123 });

const worker = new BroadcastWorker('events', async (job) => {
  console.log('Order event:', job.data);
}, {
  connection,
  subscription: 'order-handler',
  subjects: ['orders.*'],    // matches orders.created, orders.updated
  concurrency: 5,
  startFrom: '$',            // new messages only ('0-0' for replay)
});
```

**Filter syntax**: `*` matches one dot-separated token, `>` matches one or more (final position only).

---

## AI-Native Primitives

glide-mq ships seven built-in features for LLM pipeline orchestration:

### 1. Cost Tracking

```typescript
const worker = new Worker('ai', async (job) => {
  const result = await callLLM(job.data.prompt);
  await job.reportUsage({
    model: 'gpt-5.4',
    tokens: { input: 50, output: 200 },
    costs: { total: 0.003 },
  });
  return result;
});

// Aggregate across a flow
const usage = await queue.getFlowUsage(parentJobId);
const summary = await queue.getUsageSummary();
```

### 2. Token Streaming

```typescript
// Producer side
const worker = new Worker('ai', async (job) => {
  for await (const chunk of llmStream(job.data.prompt)) {
    await job.stream(chunk);  // publish incremental output
  }
});

// Consumer side
const stream = await queue.readStream(jobId);  // long-polling consumption
```

### 3. Suspend/Resume

```typescript
const worker = new Worker('approval', async (job) => {
  await sendForReview(job.data);
  await job.suspend({ reason: 'awaiting-human-review', timeout: 86400000 });
  // Processor re-invokes after signal
  const decision = job.data.signal;
  return decision;
});

// External trigger
await queue.signal(jobId, 'approved', { reviewer: 'alice' });
```

### 4. Fallback Chains

```typescript
await queue.add('inference', { prompt: 'Explain queues' }, {
  fallbacks: [
    { model: 'gpt-5.4', provider: 'openai' },
    { model: 'claude-opus-4', provider: 'anthropic' },
  ],
  lockDuration: 120000,
});

// In worker: job.currentFallback advances on retry
```

### 5. TPM Rate Limiting

```typescript
const worker = new Worker('ai', processor, {
  connection,
  tokenLimiter: { maxTokens: 100000, duration: 60000 },  // 100k TPM
});
```

### 6. Budget Caps

```typescript
await flow.add({
  name: 'research-pipeline',
  data: {},
  opts: { budget: { maxTotalTokens: 500000, maxTotalCost: 5.00 } },
  children: [/* ... */],
});
```

### 7. Per-Job Lock Duration

```typescript
await queue.add('long-inference', data, { lockDuration: 300000 });  // 5 min
```

---

## Advanced Features

### Deduplication

Three modes prevent duplicate job processing:

| Mode | Behavior |
|---|---|
| `simple` | Skip if non-terminal job with same dedup ID exists |
| `throttle` | Skip if last job with same ID was created within TTL window |
| `debounce` | Cancel previous delayed/prioritized job, create new one |

```typescript
await queue.add('webhook', data, {
  deduplication: { id: 'webhook-user-42', ttl: 60000, mode: 'throttle' },
});
```

### Batch Processing

Workers can process multiple jobs at once for I/O optimization:

```typescript
const worker = new Worker('batch-queue', async (jobs) => {
  // jobs is an array
  const results = await bulkInsertDB(jobs.map(j => j.data));
  return results;
}, { connection, batch: { size: 50, timeout: 1000 } });
```

### Ordering and Group Concurrency

```typescript
await queue.add('task', data, {
  ordering: { key: 'client-123', concurrency: 2 },
});
```

This ensures at most 2 concurrent jobs for `client-123`, with FIFO ordering within the group. This feature is free in glide-mq; in BullMQ it requires a Pro license.

### Rate Limiting

Dual-axis control:
- **RPM**: `limiter: { max: 100, duration: 60000 }` on Worker
- **TPM**: `tokenLimiter: { maxTokens: 100000, duration: 60000 }` on Worker
- **Per-group**: sliding window and token bucket per ordering key
- **Global**: `queue.rateLimitGroup()` for runtime adjustments

### Dead Letter Queues

```typescript
const worker = new Worker('tasks', processor, {
  connection,
  deadLetterQueue: { queueName: 'tasks-dlq', maxRetries: 5 },
});
```

### Step Jobs

Multi-step processing within a single job, persisting state between steps:

```typescript
const worker = new Worker('drip-campaign', async (job) => {
  switch (job.data.step) {
    case undefined:
      await sendEmail(job.data);
      await job.moveToDelayed(Date.now() + 86400000, 'check');
      return;  // pauses for 24 hours
    case 'check':
      if (!await wasEmailOpened(job.data)) {
        await job.moveToDelayed(Date.now() + 3600000, 'followup');
        return;
      }
      return { opened: true };
    case 'followup':
      await sendFollowUp(job.data);
      return { followedUp: true };
  }
});
```

### Compression

```typescript
const queue = new Queue('tasks', { connection, compression: 'gzip' });
```

Format: `gz:` + base64(gzip(data)). Maximum payload 1 MB before compression.

### Custom Serializers

Pluggable serialization (e.g., MessagePack) for reduced payload size.

---

## Observability

### OpenTelemetry

Automatic span emission when `@opentelemetry/api` is installed. No code changes required. Spans include queue name, job ID, delay, priority.

```typescript
import { setTracer, isTracingEnabled } from 'glide-mq';
setTracer(myCustomTracer);
```

### Metrics

```typescript
const counts = await queue.getJobCounts();
// { waiting: 5, active: 2, delayed: 1, completed: 100, failed: 3 }

const metrics = await queue.getMetrics('completed');
// Per-minute count and avgDuration, 24-hour retention
```

Metrics are recorded server-side inside Valkey Functions with zero extra RTTs.

### Job Logging

```typescript
// Inside processor
await job.log('Step 1 complete');
await job.log('Processing item 42');

// Outside
const logs = await queue.getJobLogs(jobId);
```

### Dashboard

`@glidemq/dashboard` provides a web UI for real-time queue inspection. The HTTP proxy exposes REST endpoints and three SSE surfaces for live streaming.

---

## Testing

glide-mq provides in-memory testing without requiring a Valkey server:

```typescript
import { TestQueue, TestWorker } from 'glide-mq/testing';

const queue = new TestQueue('tasks');
const worker = new TestWorker(queue, async (job) => {
  return { processed: job.data };
});

await queue.add('test-job', { x: 1 });

// Wait for completion
worker.on('completed', (job, result) => {
  expect(result.processed.x).toBe(1);
});

// Search jobs
const failed = await queue.searchJobs({ state: 'failed', data: { userId: 42 } });
```

TestQueue/TestWorker support retries, batch mode, custom job IDs, and event emission -- mirroring production behavior.

---

## Migration from BullMQ

### Connection Format

```typescript
// BullMQ
{ host: 'localhost', port: 6379 }

// glide-mq
{ addresses: [{ host: 'localhost', port: 6379 }] }
```

### Group/Ordering (Breaking Change)

```typescript
// BullMQ (Pro)
{ group: { id: 'client-123', limit: { max: 2, duration: 0 } } }

// glide-mq (free)
{ ordering: { key: 'client-123', concurrency: 2 } }
```

### Identical APIs (No Changes Needed)

- Exponential backoff: `backoff: { type: 'exponential', delay: 1000 }`
- Retry tracking: `job.attemptsMade`
- Job data updates: `job.updateData()`
- Worker rate limiting: `limiter: { max: 2, duration: 100 }`
- Dynamic rate limits: `worker.rateLimit()` and `Worker.RateLimitError()`

### Why Migrate

| Aspect | BullMQ | glide-mq |
|---|---|---|
| Client | ioredis (JavaScript) | valkey-glide (Rust NAPI) |
| RTTs per job | 2-3 | 1 (consolidated FCALL) |
| Server logic | 53 Lua scripts | 1 persistent function library |
| Group concurrency | Pro license | Free (Apache 2.0) |
| AI primitives | None | 7 built-in |
| Cluster support | Manual | Zero-config hash-tagged keys |

---

## Cross-Language Wire Protocol

Non-Node.js languages can interact with glide-mq queues via standard Valkey commands:

1. Load the function library (FUNCTION LOAD)
2. Call `FCALL glidemq_addJob 4 [keys] [21 args]` to enqueue
3. Read job state via `HGETALL glide:{queueName}:job:{id}`
4. Decompress if data starts with `gz:`

An HTTP proxy is also available for edge runtimes without NAPI support.

---

## Ecosystem

| Package | Purpose |
|---|---|
| `glide-mq` | Core library |
| `@glidemq/speedkey` | Underlying Valkey GLIDE client |
| `@glidemq/dashboard` | Web UI for metrics and job management |
| `@glidemq/hono` | Hono framework integration |
| `@glidemq/fastify` | Fastify framework integration |
| `@glidemq/nestjs` | NestJS framework integration |
| `@glidemq/hapi` | Hapi framework integration |

**Documentation site**: [glidemq.dev](https://www.glidemq.dev)

---

## Common Pitfalls

| Pitfall | Why It Happens | How to Avoid |
|---|---|---|
| Non-idempotent processors | At-least-once delivery means jobs may re-run after crashes | Design processors to be idempotent; use deduplication IDs for side effects |
| Missing Valkey persistence | Queue data lost on restart if AOF/RDB not configured | Enable AOF with `appendfsync everysec` minimum for production |
| Ordering deadlocks (pre-0.15.1) | Debounce cancelling ordered jobs created sequence gaps | Upgrade to 0.15.1+; the fix reloads automatically |
| Connection config format | BullMQ uses `{ host, port }`, glide-mq uses `{ addresses: [...] }` | Update all connection configs during migration |
| Browser usage | Requires NAPI runtime (Node.js 20+, Bun, Deno) | Use HTTP proxy for edge/browser environments |
| Kafka expectations | glide-mq is a task queue, not a partitioned event log | Use Kafka/Redpanda for event streaming; glide-mq for job processing |
| Async replication data loss | Valkey primary failure can lose un-replicated writes | Configure AOF + replication; accept at-least-once semantics |

---

## Performance Benchmarks

Tested on AWS ElastiCache Valkey 8.2 (r7g.large) with TLS:

| Concurrency | glide-mq (j/s) | BullMQ (j/s) | Gain |
|---|---|---|---|
| c=5 | 10,754 | 9,865 | +9% |
| c=10 | 18,218 | 13,541 | +35% |
| c=15 | 19,583 | 14,162 | +38% |
| c=20 | 19,408 | 16,039 | +21% |

The advantage comes from 1-RTT-per-job; gains compound under network latency.

---

## Version History (Key Releases)

| Version | Date | Highlights |
|---|---|---|
| 0.15.1 | 2026-04-06 | Debounce + ordering deadlock fix |
| 0.15.0 | 2026-04-02 | HTTP proxy, Flow API, SSE |
| 0.14.0 | 2026-03-28 | Usage/cost tracking redesign |
| 0.13.0 | 2026-03-27 | Suspend/resume, streaming, vector search |
| 0.12.0 | 2026-03-20 | Runtime rate limiting, ordering unification |
| 0.10.0 | 2026-03-09 | 108% throughput gain at c=1 |
| 0.9.0 | 2026-03-08 | Producer, ServerlessPool, LIFO, DAGs |

**Current version**: 0.15.1 (Apache 2.0 license)

---

## When NOT to Use glide-mq

- **Event streaming**: No partitions, offset management, or replay (use Kafka)
- **Browser environments**: Requires server-side NAPI runtime
- **Exactly-once delivery**: Provides at-least-once only
- **No Valkey/Redis available**: Requires a backing store (no embedded option for production)

---

## Further Reading

| Resource | Type | Why Recommended |
|---|---|---|
| [GitHub: avifenesh/glide-mq](https://github.com/avifenesh/glide-mq) | Repository | Source code, issues, changelog |
| [glidemq.dev](https://www.glidemq.dev) | Documentation | Full API reference and guides |
| [USAGE.md](https://github.com/avifenesh/glide-mq/blob/main/docs/USAGE.md) | Guide | Queue, Worker, Producer API details |
| [WORKFLOWS.md](https://github.com/avifenesh/glide-mq/blob/main/docs/WORKFLOWS.md) | Guide | FlowProducer, DAGs, chains, groups |
| [ADVANCED.md](https://github.com/avifenesh/glide-mq/blob/main/docs/ADVANCED.md) | Guide | Schedulers, rate limiting, retries |
| [ARCHITECTURE.md](https://github.com/avifenesh/glide-mq/blob/main/docs/ARCHITECTURE.md) | Guide | Internal data structures and design |
| [MIGRATION.md](https://github.com/avifenesh/glide-mq/blob/main/docs/MIGRATION.md) | Guide | BullMQ migration path |
| [WIRE_PROTOCOL.md](https://github.com/avifenesh/glide-mq/blob/main/docs/WIRE_PROTOCOL.md) | Spec | Cross-language interop |
| [SERVERLESS.md](https://github.com/avifenesh/glide-mq/blob/main/docs/SERVERLESS.md) | Guide | Lambda/Edge deployment patterns |
| [TESTING.md](https://github.com/avifenesh/glide-mq/blob/main/docs/TESTING.md) | Guide | In-memory TestQueue/TestWorker |
| [DURABILITY.md](https://github.com/avifenesh/glide-mq/blob/main/docs/DURABILITY.md) | Guide | At-least-once guarantees |
| [BROADCAST.md](https://github.com/avifenesh/glide-mq/blob/main/docs/BROADCAST.md) | Guide | Pub/sub with subject filtering |
| [OBSERVABILITY.md](https://github.com/avifenesh/glide-mq/blob/main/docs/OBSERVABILITY.md) | Guide | OpenTelemetry, metrics, dashboard |
| [STEP_JOBS.md](https://github.com/avifenesh/glide-mq/blob/main/docs/STEP_JOBS.md) | Guide | Multi-step job processing |
| [Valkey GLIDE](https://github.com/valkey-io/valkey-glide) | Dependency | Underlying client library |
| [Valkey Streams](https://valkey.io/topics/streams-intro/) | Reference | Stream primitives that power glide-mq |
| [Valkey Functions](https://valkey.io/topics/functions-intro/) | Reference | Persistent server-side functions |
| [BullMQ Docs](https://docs.bullmq.io/) | Comparison | Primary alternative for comparison |
| [npm: glide-mq](https://www.npmjs.com/package/glide-mq) | Package | npm listing and version info |
| [CHANGELOG.md](https://github.com/avifenesh/glide-mq/blob/main/CHANGELOG.md) | Reference | Version history and breaking changes |
| [avifenesh profile](https://github.com/avifenesh) | Author | Creator and maintainer context |
| [Valkey blog](https://valkey.io/blog/) | Blog | Valkey ecosystem updates |

---

*This guide was synthesized from 22 sources. See `resources/glide-mq-sources.json` for full source list with quality scores.*
