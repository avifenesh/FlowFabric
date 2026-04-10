# Learning Guide: BullMQ Pro - Premium Redis/Valkey-Based Job Queue for Node.js

**Generated**: 2026-04-09
**Sources**: 42 resources analyzed
**Depth**: deep

---

## Prerequisites

- Working knowledge of Node.js and async/await patterns
- Basic understanding of Redis data structures (strings, lists, hashes, sorted sets, streams)
- Familiarity with message queue concepts (producers, consumers, jobs, workers)
- Node.js 18+ and a Redis 6.2+ (or Valkey, DragonflyDB) instance
- An active BullMQ Pro subscription from taskforce.sh (for Pro features)

## TL;DR

- BullMQ is the leading Redis-based job queue for Node.js, built on Redis Streams and Lua scripts for atomicity, supporting delayed jobs, priorities, retries, flows, rate limiting, and sandboxed processors.
- BullMQ Pro extends open-source BullMQ with **Groups** (virtual queues with round-robin, per-group concurrency and rate limiting), **Batches** (process multiple jobs at once), **Observables** (RxJS-based cancellation and TTL), and professional support.
- Architecture: jobs traverse a state machine (wait -> active -> completed/failed) backed by Redis sorted sets, streams, and hashes; all state transitions are atomic via Lua scripts.
- Pricing: Standard $1,395/year ($139/month) per deployment; Enterprise and Embedded tiers available.
- Compatible with Redis 6.2+, Valkey, DragonflyDB, AWS ElastiCache, and Upstash.

---

## 1. Architecture and Internals

### 1.1 Core Design Principles

BullMQ is described as a "fast and robust queue system built on top of Redis" targeting four goals:

1. **Exactly-once semantics** -- at minimum, at-least-once delivery
2. **Horizontal scalability** -- parallel worker processing across machines
3. **Consistency** -- atomic operations via Lua scripts and Redis pipelining
4. **High performance** -- polling-free design minimizing CPU consumption

All queue operations are implemented as Lua scripts executed atomically on Redis, preventing race conditions even with multiple distributed workers.

### 1.2 Job Lifecycle State Machine

Jobs traverse these states:

```
                    +---> prioritized ---+
                    |                    |
  add() ---> wait --+------------------->+---> active ---> completed
                    |                    |       |
                    +---> delayed -------+       +-------> failed
                    |                                        |
                    +--- waiting-children ---+     retry ----+
                         (flows only)        |
                                             v
                                          wait/delayed/prioritized
```

**States:**
- **wait**: Standard entry point; FIFO ordering
- **prioritized**: Jobs with priority > 0; O(log n) insertion using sorted sets
- **delayed**: Jobs with a delay; stored in a sorted set keyed by execution timestamp
- **active**: Currently being processed; protected by a distributed lock
- **waiting-children**: (Flows) Parent job waiting for all children to complete
- **completed**: Successfully processed
- **failed**: Threw an exception during processing

Priority values range from 0 to 2^21 (2,097,152), where lower numbers mean higher priority (Unix convention). Jobs without explicit priority get highest priority (value 0).

### 1.3 Redis Data Structures Used

| Structure | Purpose |
|-----------|---------|
| Redis Streams | Event system (QueueEvents); reliable delivery without pub-sub message loss |
| Sorted Sets | Delayed jobs (scored by timestamp), prioritized jobs (scored by priority) |
| Lists | Wait queue (FIFO/LIFO ordering) |
| Hashes | Job data storage, metadata, metrics counters |
| Strings | Lock keys, rate limiter counters, global concurrency |

### 1.4 Connection Model

BullMQ uses **ioredis** under the hood. Key connection considerations:

- Default: localhost:6379
- `maxRetriesPerRequest` **must be set to `null`** for Worker instances (enables indefinite retries)
- Queue (producer) instances can use default retry settings for fail-fast behavior
- QueueEvents requires its own blocking connection (cannot reuse)
- **Never** use ioredis `keyPrefix` -- BullMQ has its own prefix mechanism
- Redis must be configured with `maxmemory-policy=noeviction`

```typescript
import { Queue, Worker } from 'bullmq';
import IORedis from 'ioredis';

const connection = new IORedis({
  host: 'localhost',
  port: 6379,
  maxRetriesPerRequest: null, // required for workers
});

const queue = new Queue('myQueue', { connection });
const worker = new Worker('myQueue', processor, { connection });
```

---

## 2. Core API (Open Source)

### 2.1 Queue

The Queue class is lightweight, primarily used for adding jobs:

```typescript
const queue = new Queue('Cars');

// Basic job
await queue.add('paint', { color: 'red' });

// Delayed job (5 seconds)
await queue.add('paint', { color: 'blue' }, { delay: 5000 });

// Prioritized job
await queue.add('paint', { color: 'gold' }, { priority: 1 });

// With auto-removal
await queue.add('paint', { color: 'green' }, {
  removeOnComplete: { count: 1000 },
  removeOnFail: { age: 86400 }, // 24 hours
});
```

When a Queue is instantiated, BullMQ performs an upsert on a Redis meta-key, allowing reconnection to existing queues.

### 2.2 Worker

Workers are the processing engine:

```typescript
const worker = new Worker('Cars', async (job) => {
  // job.name, job.data, job.id available
  await paintCar(job.data.color);
  await job.updateProgress(50);
  return { painted: true };
}, {
  connection,
  concurrency: 50, // process up to 50 jobs concurrently
});

// CRITICAL: Always attach error handler
worker.on('error', (err) => console.error(err));

// Event listeners
worker.on('completed', (job, result) => { /* ... */ });
worker.on('failed', (job, err) => { /* ... */ });
worker.on('progress', (job, progress) => { /* ... */ });
```

**Concurrency**: The concurrency option controls parallel job execution per worker instance. This only works for I/O-bound operations (async/await). For CPU-intensive work, use sandboxed processors or multiple worker processes. Typical I/O concurrency: 100-300.

**Auto-run control**:

```typescript
const worker = new Worker(queueName, processor, { autorun: false });
// Start when ready
worker.run();
```

### 2.3 QueueEvents

Global event monitoring across all workers, implemented via Redis Streams (not pub-sub) for reliable delivery:

```typescript
const queueEvents = new QueueEvents('Cars');

queueEvents.on('completed', ({ jobId, returnvalue }) => {
  console.log(`Job ${jobId} completed with ${returnvalue}`);
});

queueEvents.on('failed', ({ jobId, failedReason }) => {
  console.error(`Job ${jobId} failed: ${failedReason}`);
});
```

Event streams auto-trim at ~10,000 events. Manual trim: `await queue.trimEvents(100)`.

### 2.4 FlowProducer

Creates hierarchical parent-child job dependencies:

```typescript
import { FlowProducer } from 'bullmq';

const flow = new FlowProducer({ connection });

const tree = await flow.add({
  name: 'renovate-interior',
  queueName: 'renovate',
  children: [
    { name: 'paint-ceiling', queueName: 'steps', data: { color: 'white' } },
    { name: 'paint-walls', queueName: 'steps', data: { color: 'blue' } },
    { name: 'fix-floor', queueName: 'steps', data: {} },
  ],
});
```

**Key behaviors:**
- Parent stays in `waiting-children` state until all children complete
- Children can be in different queues than the parent
- All jobs added atomically
- Parent accesses child results via `job.getChildrenValues()`
- Parent removal cascades to all descendants
- Supports arbitrary-depth tree structures

**Querying flow trees:**

```typescript
const tree = await flow.getFlow({
  id: topJob.id,
  queueName: 'topQueueName',
  depth: 2,        // limit depth
  maxChildren: 10, // limit children per node
});
```

---

## 3. Advanced Features (Open Source)

### 3.1 Rate Limiting

Global rate limiting across all workers for a queue:

```typescript
const worker = new Worker('painter', async (job) => paintCar(job), {
  connection,
  limiter: {
    max: 10,      // 10 jobs
    duration: 1000, // per second
  },
});
```

Rate-limited jobs stay in the `waiting` state (not a separate state).

**Manual/Dynamic rate limiting** (e.g., responding to HTTP 429):

```typescript
const worker = new Worker('api-calls', async (job) => {
  const [isLimited, retryAfter] = await callExternalAPI();
  if (isLimited) {
    await worker.rateLimit(retryAfter);
    throw Worker.RateLimitError();
  }
}, {
  connection,
  limiter: { max: 100, duration: 1000 }, // required even for manual
});
```

**Burst smoothing recipe:** Convert `max: 300, duration: 1000` to `max: 1, duration: 3.33` for even distribution.

### 3.2 Retries and Backoff

```typescript
await queue.add('send-email', data, {
  attempts: 5,
  backoff: {
    type: 'exponential', // or 'fixed'
    delay: 1000,         // base delay in ms
    jitter: 0.5,         // optional randomness (0-1)
  },
});
```

**Exponential**: delay = 2^(attempts-1) * baseDelay (1s, 2s, 4s, 8s, 16s)
**Fixed**: retries at constant baseDelay intervals

**Custom backoff strategy:**

```typescript
const worker = new Worker('myQueue', processor, {
  settings: {
    backoffStrategy: (attemptsMade, type, err, job) => {
      if (type === 'custom') {
        return attemptsMade * 2000; // custom delay
      }
      return -1; // -1 = fail permanently
    },
  },
});
```

### 3.3 Job Schedulers (Repeatable Jobs)

As of BullMQ 5.16, Job Schedulers replace the deprecated repeatable jobs API:

```typescript
// Cron-based scheduler
const job = await queue.upsertJobScheduler(
  'daily-report',
  { pattern: '0 15 3 * * *' }, // 3:15 AM daily
  {
    name: 'generate-report',
    data: { type: 'daily' },
    opts: { attempts: 3, backoff: { type: 'exponential', delay: 1000 } },
  },
);

// Interval-based scheduler
await queue.upsertJobScheduler(
  'health-check',
  { every: 30000 }, // every 30 seconds
);
```

The `upsert` approach prevents duplicate schedulers in production. The scheduler only generates a new job when the previous one begins processing.

**Legacy repeatable jobs (still functional):**

```typescript
await queue.add('submarine', { color: 'yellow' }, {
  repeat: { pattern: '0 15 3 * * *' },
});

// Custom repeat key for updates
await queue.add('bird', data, {
  repeat: { every: 10000, key: 'eagle' },
});
```

### 3.4 Delayed Jobs

```typescript
// Delay by milliseconds
await queue.add('notification', data, { delay: 5000 });

// Schedule for specific time
const target = new Date('2026-12-25T00:00:00');
await queue.add('christmas', data, {
  delay: Number(target) - Date.now(),
});

// Reschedule a delayed job
await job.changeDelay(10000);
```

Jobs are stored in a sorted set keyed by execution timestamp. Execution time is not precisely guaranteed but is typically accurate.

### 3.5 Prioritized Jobs

```typescript
await queue.add('urgent', data, { priority: 1 });   // high
await queue.add('normal', data, { priority: 100 });  // lower
await queue.add('batch', data);                       // no priority = highest

// Change priority after creation
await job.changePriority({ priority: 1 });

// Query counts per priority
const counts = await queue.getCountsPerPriority([0, 1, 5, 10]);
```

Adding prioritized jobs is O(log n) relative to existing prioritized jobs.

### 3.6 Job Deduplication

Three modes for preventing duplicate processing:

**Simple mode** (deduplicate until job completes/fails):

```typescript
await queue.add('upload', data, {
  deduplication: { id: 'file-abc123' },
});
```

**Throttle mode** (TTL-based window):

```typescript
await queue.add('update-profile', data, {
  deduplication: { id: 'user-42', ttl: 5000 },
});
```

**Debounce mode** (replace previous, reset TTL):

```typescript
await queue.add('search-index', data, {
  deduplication: { id: 'reindex', ttl: 10000, extend: true, replace: true },
});
```

**Keep-last-if-active** (ensures latest data processes after current job):

```typescript
await queue.add('sync', data, {
  deduplication: { id: 'sync-1', keepLastIfActive: true },
});
```

### 3.7 Sandboxed Processors

For CPU-intensive work that would block the event loop and cause stalled jobs:

```typescript
// processor.ts (separate file)
import { SandboxedJob } from 'bullmq';

export default async function (job: SandboxedJob) {
  // Heavy CPU work here -- runs in isolated process
  return result;
}

// main.ts
import { Worker } from 'bullmq';
import path from 'path';

const worker = new Worker('heavy-queue',
  path.join(__dirname, 'processor.js'),
  { connection }
);

// Or use worker threads (since v3.13.0, lower overhead)
const worker2 = new Worker('heavy-queue',
  path.join(__dirname, 'processor.js'),
  { connection, useWorkerThreads: true }
);
```

### 3.8 Stalled Jobs

When a worker cannot renew its lock within ~30 seconds (default), the job is declared stalled:

- **First stall**: Job returns to `waiting` state for reprocessing
- **Exceeds maxStalledCount** (default 1): Job moves to `failed` with error "job stalled more than allowable limit"

Prevention:
1. Keep CPU operations under 30 seconds
2. Use sandboxed processors for heavy work
3. Monitor `worker.on('stalled', ...)` events

### 3.9 Global Concurrency

Limits total concurrent jobs across ALL workers for a queue:

```typescript
await queue.setGlobalConcurrency(4);

// Query
const gc = await queue.getGlobalConcurrency();

// Remove
await queue.removeGlobalConcurrency();
```

Worker-level concurrency acts as a secondary limit that cannot exceed the global threshold.

### 3.10 Concurrency Hierarchy

| Level | Scope | How to Set |
|-------|-------|-----------|
| Worker concurrency | Per worker instance | `new Worker(name, fn, { concurrency: 50 })` |
| Global concurrency | All workers on queue | `queue.setGlobalConcurrency(N)` |
| Group concurrency (Pro) | Per group across all workers | `WorkerPro({ group: { concurrency: N } })` |
| Local group concurrency (Pro) | Per individual group | `queue.setGroupConcurrency(groupId, N)` |

### 3.11 Queue-Level Job Management

**Drain** (remove waiting/delayed, keep active/completed/failed):

```typescript
await queue.drain();
```

**Clean** (remove by state with grace period):

```typescript
const removed = await queue.clean(60000, 100, 'completed'); // older than 60s, max 100
```

**Obliterate** (destroy entire queue):

```typescript
await queue.obliterate({ force: true }); // even if jobs are active
```

**Auto-removal:**

```typescript
// Count-based
{ removeOnComplete: 1000, removeOnFail: 5000 }

// Age-based (seconds)
{ removeOnComplete: { age: 3600 }, removeOnFail: { age: 86400, count: 1000 } }

// Immediate removal
{ removeOnComplete: true, removeOnFail: true }
```

### 3.12 Metrics

```typescript
const worker = new Worker('myQueue', processor, {
  metrics: { maxDataPoints: 20160 }, // 2 weeks at 1-min intervals (~120KB RAM)
});

// Query metrics
const completed = await queue.getMetrics('completed', 0, 59); // last hour
const failed = await queue.getMetrics('failed');
```

### 3.13 OpenTelemetry Integration

BullMQ includes a Telemetry interface supporting the OpenTelemetry specification for distributed tracing. As of v5.71.0, gauge metrics for counting jobs by state are also supported.

### 3.14 Process Step Jobs (Multi-Step Pattern)

```typescript
const worker = new Worker('multi-step', async (job, token) => {
  let step = job.data.step || 'initial';

  switch (step) {
    case 'initial':
      await doStep1();
      await job.updateData({ ...job.data, step: 'process' });
      await job.moveToDelayed(Date.now() + 1000, token);
      throw new DelayedError();

    case 'process':
      await doStep2();
      await job.updateData({ ...job.data, step: 'finalize' });
      // Add children dynamically
      await queue.add('child-task', childData, { parent: { id: job.id, queue: job.queueQualifiedName } });
      if (await job.moveToWaitingChildren(token)) {
        throw new WaitingChildrenError();
      }
      break;

    case 'finalize':
      return await doStep3();
  }
});
```

---

## 4. BullMQ Pro Features

### 4.1 Installation

BullMQ Pro is distributed via a private npm registry:

```bash
# .npmrc
@taskforcesh:registry=https://npm.taskforce.sh/
//npm.taskforce.sh/:_authToken=${NPM_TASKFORCESH_TOKEN}
always-auth=true
```

```bash
yarn add @taskforcesh/bullmq-pro
# or: npm install @taskforcesh/bullmq-pro
```

```typescript
import { QueuePro, WorkerPro } from '@taskforcesh/bullmq-pro';
```

Pro classes extend the open-source classes, so all base functionality is available.

### 4.2 Groups

Groups enable distributing jobs across logical collections within a single queue, processed in round-robin order. They function as "virtual queues" with zero Redis overhead when empty.

**Core concept:** A video transcoding service where each user is a group. Without groups, one prolific user monopolizes the queue. With groups, fair round-robin distribution ensures all users get service.

```typescript
const queue = new QueuePro('transcoding', { connection });

// Add jobs to groups
await queue.add('transcode', videoData1, { group: { id: 'user-1' } });
await queue.add('transcode', videoData2, { group: { id: 'user-2' } });
await queue.add('transcode', videoData3, { group: { id: 'user-1' } });

// Worker processes in round-robin across groups
const worker = new WorkerPro('transcoding', async (job) => {
  await transcodeVideo(job.data);
}, { connection });
```

**Key properties:**
- Unlimited groups supported with no performance impact
- Non-grouped jobs in the same queue get priority over grouped jobs
- Waiting list per group holds only the next job to process (memory efficient)

#### 4.2.1 Group Concurrency

**Global group concurrency** (same limit for all groups):

```typescript
const worker = new WorkerPro('myQueue', processor, {
  group: { concurrency: 3 },  // max 3 active jobs per group
  concurrency: 100,            // worker-level concurrency
  connection,
});
```

This is enforced globally: regardless of how many workers exist, no group processes more than 3 jobs concurrently.

**Local (per-group) concurrency** (different limits per group):

```typescript
const queue = new QueuePro('myQueue', { connection });

await queue.setGroupConcurrency('premium-users', 10);
await queue.setGroupConcurrency('free-users', 2);

// Query
const concurrency = await queue.getGroupConcurrency('premium-users'); // 10
```

Local concurrency values persist in Redis and must be cleaned up manually for unused groups. Global group concurrency must also be set at the worker level as a default.

#### 4.2.2 Group Rate Limiting

```typescript
const worker = new WorkerPro('api-calls', processor, {
  group: {
    limit: {
      max: 100,      // 100 jobs per group
      duration: 1000, // per second
    },
  },
  connection,
});
```

When a group exceeds its rate limit, only that group pauses; others continue normally.

**Manual group rate limiting** (for 429 responses):

```typescript
const worker = new WorkerPro('api-calls', async (job) => {
  const response = await callAPI(job.data);
  if (response.status === 429) {
    await worker.rateLimitGroup(job, response.headers['retry-after'] * 1000);
    throw Worker.RateLimitError();
  }
}, { connection, group: { limit: { max: 50, duration: 1000 } } });

// Check rate limit status
const ttl = await queue.getGroupRateLimitTtl('group-1');
```

#### 4.2.3 Group Priority

Jobs within a group can be prioritized:

```typescript
await queue.add('task', data, {
  group: { id: 'group-1', priority: 10 },
});

// Query priority distribution
const counts = await queue.getCountsPerPriorityForGroup('group-1', [0, 1, 5, 10]);
```

Priority 0 = highest (same Unix convention as base BullMQ).

#### 4.2.4 Max Group Size

Limit the number of queued jobs per group:

```typescript
try {
  await queue.add('paint', data, {
    group: { id: 'group-1', maxSize: 100 },
  });
} catch (err) {
  if (err instanceof GroupMaxSizeExceededError) {
    console.log('Group full, job discarded');
  }
}
```

Not compatible with `addBulk`.

#### 4.2.5 Group Pausing

Pause and resume individual groups without affecting others:

```typescript
await queue.pauseGroup('maintenance-group');
// Workers finish current jobs from this group, then stop picking new ones

await queue.resumeGroup('maintenance-group');
// Returns false if group doesn't exist or already active
```

#### 4.2.6 Group Getters

```typescript
// Total job count across groups
const total = await queue.getGroupsJobsCount(1000); // 1000 groups/iteration

// Active count for a specific group
const active = await queue.getGroupActiveCount('group-1');

// Get jobs with pagination
const jobs = await queue.getGroupJobs('group-1', 0, 100);
```

### 4.3 Batches

Process multiple jobs simultaneously for higher throughput:

```typescript
const worker = new WorkerPro('batch-queue', async (job) => {
  const batch = job.getBatch();
  for (const batchedJob of batch) {
    try {
      await processItem(batchedJob.data);
    } catch (err) {
      batchedJob.setAsFailed(err); // fail individual jobs, not whole batch
    }
  }
}, {
  batch: { size: 25 },      // max 25 jobs per batch
  connection,
});
```

**Advanced batch configuration:**

```typescript
{
  batch: {
    size: 50,        // max jobs per batch
    minSize: 10,     // wait for at least 10 jobs
    timeout: 5000,   // max wait time for minSize (ms)
  },
}
```

**Important constraints:**
- `minSize` and `timeout` are NOT compatible with groups
- Default: if processing throws, ALL batch jobs fail (use `setAsFailed()` for granular control)
- Worker-level events report for the dummy batch job; use `QueueEventsPro` for individual job events
- Not compatible with dynamic rate limiting, manual processing, or dynamic delays
- Optimal batch sizes: 10-50 jobs (larger introduces proportional overhead)

### 4.4 Observables

Return RxJS Observables instead of Promises from workers for advanced patterns:

```typescript
import { Observable } from 'rxjs';
import { WorkerPro } from '@taskforcesh/bullmq-pro';

const worker = new WorkerPro('observable-queue', async (job) => {
  return new Observable((subscriber) => {
    let step = 0;

    const interval = setInterval(() => {
      subscriber.next({ step: ++step }); // emit progress
      if (step >= 10) {
        subscriber.complete();
      }
    }, 1000);

    // Cleanup on cancellation
    return () => {
      clearInterval(interval);
      console.log('Job cancelled, resources cleaned up');
    };
  });
}, { connection });
```

**Key advantages over Promises:**
- **Multiple emissions**: Observables can emit intermediate values
- **Cancellable**: Cleanup function runs on cancellation (timers, connections, etc.)
- **State resumption**: Last emitted value is persisted as `job.returnvalue`, enabling checkpoint/resume patterns

#### 4.4.1 TTL-Based Cancellation

```typescript
// Global TTL for all jobs
const worker = new WorkerPro('myQueue', processor, {
  ttl: 30000, // 30 second max processing time
  connection,
});

// Per-job-name TTL
const worker = new WorkerPro('myQueue', processor, {
  ttl: {
    'fast-task': 5000,
    'slow-task': 120000,
  },
  connection,
});
```

### 4.5 Professional Support

- Email support at support@taskforce.sh
- Target response: 1 business day
- Resolution time varies by complexity
- Access to BullMQ maintainers from Taskforce.sh

---

## 5. Patterns and Best Practices

### 5.1 Named Processor Pattern

```typescript
const worker = new Worker('tasks', async (job) => {
  switch (job.name) {
    case 'send-email':
      return await sendEmail(job.data);
    case 'generate-pdf':
      return await generatePDF(job.data);
    case 'resize-image':
      return await resizeImage(job.data);
    default:
      throw new Error(`Unknown job type: ${job.name}`);
  }
}, { connection });
```

### 5.2 Redis Cluster Configuration

BullMQ requires all keys for a queue to be on the same cluster node. Use hash tags:

```typescript
// Method 1: Custom prefix
const queue = new Queue('myQueue', { prefix: '{myprefix}' });

// Method 2: Wrapped queue name
const queue = new Queue('{myQueue}');
```

For multiple queues, use distinct prefixes to distribute across nodes.

### 5.3 Async Job Pattern (Do Not Block on Completion)

Avoid `waitUntilFinished()` in production due to:
1. No completion guarantees if handler fails after job completes
2. Unpredictable duration under load
3. Memory/CPU overhead from promises and listeners

Instead:
- Return a job ID immediately to the caller
- Use polling or webhooks for completion notification
- Queue secondary notification jobs for critical workflows

### 5.4 Webhook/Notification Architecture

```typescript
// Producer: enqueue and return immediately
app.post('/process', async (req, res) => {
  const job = await queue.add('process', req.body);
  res.json({ jobId: job.id, status: 'queued' });
});

// Worker: process and notify
const worker = new Worker('process', async (job) => {
  const result = await doWork(job.data);
  // Queue notification as separate reliable job
  await notifyQueue.add('notify', {
    webhookUrl: job.data.callbackUrl,
    result,
  });
  return result;
}, { connection });
```

### 5.5 Rate Limit Smoothing

```typescript
// Bursty: 300 requests hit simultaneously then wait
{ max: 300, duration: 1000 }

// Smooth: 1 request every ~3.3ms
{ max: 1, duration: 3.33 }
```

---

## 6. Production Deployment

### 6.1 Redis Configuration

```
# REQUIRED: prevent BullMQ keys from being evicted
maxmemory-policy noeviction

# RECOMMENDED: enable persistence
appendonly yes
appendfsync everysec
```

### 6.2 Connection Resilience

```typescript
const connection = new IORedis({
  host: process.env.REDIS_HOST,
  port: 6379,
  maxRetriesPerRequest: null,
  retryStrategy: (times) => Math.min(times * 1000, 20000),
  enableOfflineQueue: true, // for workers
});
```

### 6.3 Graceful Shutdown

```typescript
const shutdown = async () => {
  console.log('Shutting down gracefully...');
  await worker.close(); // waits for active jobs to finish
  await queue.close();
  process.exit(0);
};

process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
```

`worker.close()` marks the worker as closing (no new jobs picked up) and waits for current jobs to complete. If shutdown fails, remaining jobs become stalled and are picked up by other workers after ~30 seconds.

### 6.4 Job Retention Strategy

```typescript
const queue = new Queue('production', {
  defaultJobOptions: {
    removeOnComplete: { age: 3600, count: 5000 }, // 1 hour or 5000 jobs
    removeOnFail: { age: 604800, count: 10000 },  // 7 days or 10000 jobs
    attempts: 3,
    backoff: { type: 'exponential', delay: 1000 },
  },
});
```

### 6.5 Security

- Never store sensitive data directly in job payloads
- Encrypt sensitive fields before queueing if necessary
- Handle `uncaughtException` and `unhandledRejection` globally

### 6.6 Error Handling

```typescript
// CRITICAL: missing error handler may stop job processing
worker.on('error', (err) => {
  logger.error('Worker error:', err);
});

worker.on('failed', (job, err) => {
  logger.error(`Job ${job?.id} failed:`, err.message);
});

worker.on('stalled', (jobId) => {
  logger.warn(`Job ${jobId} stalled`);
});
```

---

## 7. NestJS Integration

```typescript
import { Module } from '@nestjs/common';
import { BullModule } from '@nestjs/bullmq';

@Module({
  imports: [
    BullModule.forRoot({
      connection: { host: 'localhost', port: 6379 },
    }),
    BullModule.registerQueue({ name: 'emails' }),
    BullModule.registerFlowProducer({ name: 'emailFlows' }),
  ],
})
export class AppModule {}
```

```typescript
import { Processor, WorkerHost, OnWorkerEvent } from '@nestjs/bullmq';
import { Job } from 'bullmq';

@Processor('emails')
export class EmailProcessor extends WorkerHost {
  async process(job: Job<any, any, string>): Promise<any> {
    switch (job.name) {
      case 'welcome':
        return this.sendWelcome(job.data);
      case 'reset-password':
        return this.sendPasswordReset(job.data);
    }
  }

  @OnWorkerEvent('completed')
  onCompleted() {
    // handle completion
  }
}
```

---

## 8. Redis/Valkey Compatibility

| Backend | Status | Notes |
|---------|--------|-------|
| Redis 6.2+ | Fully supported | Reference implementation |
| Valkey | Supported | Official support, regularly tested |
| DragonflyDB | Supported | Use `{queueName}` syntax for multi-core optimization |
| AWS ElastiCache | Supported | Officially supported |
| Upstash | Supported | Listed as compatible |

**DragonflyDB optimization:** Use curly braces in queue names (`{myqueue}`) so Dragonfly assigns dedicated threads per queue. Splitting across `{myqueue-1}`, `{myqueue-2}` enables multi-core distribution, but priorities and rate limiting may not work across split queues.

---

## 9. Open Source vs Pro Comparison

| Feature | Open Source (MIT) | Pro (Licensed) |
|---------|-------------------|----------------|
| Queues, Workers, Events | Yes | Yes |
| Delayed, Prioritized, Repeatable Jobs | Yes | Yes |
| Flows (Parent-Child Dependencies) | Yes | Yes |
| Global Rate Limiting | Yes | Yes |
| Retries with Backoff | Yes | Yes |
| Sandboxed Processors | Yes | Yes |
| Job Deduplication | Yes | Yes |
| Metrics, Telemetry | Yes | Yes |
| Redis Cluster Support | Yes | Yes |
| **Groups (Virtual Queues)** | No | Yes |
| **Per-Group Rate Limiting** | No | Yes |
| **Per-Group Concurrency** | No | Yes |
| **Group Priority** | No | Yes |
| **Group Pausing** | No | Yes |
| **Batches** | No | Yes |
| **Observables (RxJS)** | No | Yes |
| **TTL Cancellation** | No | Yes |
| **Professional Support** | No | Yes |
| Pricing | Free | $1,395/yr ($139/mo) Standard |

### 9.1 Pricing Details

| Tier | Cost | Scope |
|------|------|-------|
| Standard | $1,395/year ($139/month) | Single deployment; organizations <100 employees |
| Enterprise | Custom | Multiple deployments; volume discounts; priority support |
| Embedded | Custom | Product redistribution rights |

A "deployment" is a single operational environment (Kubernetes cluster, server, VM) connecting to one or more Redis instances.

---

## 10. Common Pitfalls

| Pitfall | Why It Happens | How to Avoid |
|---------|---------------|--------------|
| Missing error handler on worker | Unhandled error stops job processing | Always attach `worker.on('error', ...)` |
| `maxRetriesPerRequest` not null | ioredis default (20) causes premature worker failure | Set to `null` for Worker connections |
| Using ioredis `keyPrefix` | Conflicts with BullMQ's internal prefix | Use BullMQ's `prefix` option instead |
| Redis `maxmemory-policy` not `noeviction` | Keys get evicted, breaking queue state | Set `noeviction` in Redis config |
| CPU-heavy processors causing stalls | Event loop blocked, lock renewal fails | Use sandboxed processors or worker threads |
| Not closing workers on shutdown | Active jobs become stalled | Implement graceful shutdown with signal handlers |
| `removeOnComplete: true` breaking dedup | Removed jobs no longer tracked for uniqueness | Use count/age-based removal instead |
| Custom job IDs containing colons | Redis naming convention conflict | Use hyphens or underscores |
| Blocking on `waitUntilFinished()` | Memory leaks, unreliable in production | Use event-driven patterns instead |
| Batch `minSize` with groups | Incompatible, silently ignored | Avoid combining; worker processes available jobs without waiting |

---

## 11. Migration Notes

### From Bull to BullMQ

- BullMQ is the successor to Bull (Bull 4.x was renamed BullMQ)
- Different npm package: `bullmq` vs `bull`
- QueueScheduler removed in BullMQ 2.0+
- Named processors removed (use switch/case pattern)
- Group rate limiting (was in Bull `limiter.groupKey`) moved to Pro-only

### Version Upgrades

- Upgrade incrementally: bugfix -> minor -> major
- Bugfix releases: safe to update without code changes
- Feature releases: maintain backward compatibility with running workers
- Major releases: may have API-breaking or data-structure-breaking changes
- For risky upgrades: pause queue -> upgrade -> resume, or use new queue names

### BullMQ 5.0 Breaking Changes

- Markers use a dedicated Redis key instead of special Job ID
- Review changelog before upgrading

---

## 12. Ecosystem

### 12.1 Taskforce.sh Dashboard

Professional monitoring dashboard for BullMQ/Bull queues. Provides real-time visibility into queue health, job states, and worker performance. Built with Angular Material.

### 12.2 BullMQ Proxy

[bullmq-proxy](https://github.com/taskforcesh/bullmq-proxy) enables language-agnostic queue access for environments where direct Redis connectivity from the application is not possible.

### 12.3 Language Support

| Language | Package | Maturity |
|----------|---------|----------|
| TypeScript/Node.js | bullmq | Primary (53.6% of codebase) |
| Python | bullmq (Python) | Production-ready |
| Elixir | bullmq (Elixir) | Production-ready (32.1%) |
| PHP | bullmq (PHP) | Available |
| Bun | @taskforcesh/bullmq-pro | Supported for Pro |

### 12.4 GitHub Stats (as of April 2026)

- Stars: 8,700+
- Forks: 591
- Total Releases: 818
- Latest Version: v5.73.3
- BullMQ Pro Latest: v7.43.1
- Notable Users: Microsoft, Vendure, Datawrapper, NestJS, Langfuse, Novu

---

## 13. Recent Developments (2026)

- **v5.73.x**: Performance improvement for fetching next job when moving to delayed; sandbox getDependenciesCount proxy
- **v5.72.0**: `keepLastIfActive` deduplication option for at-least-once-after-active semantics
- **v5.71.0**: OpenTelemetry gauge metrics for job state counting
- **v5.70.0**: Cancellation support for sandboxed processors
- **Pro v7.43.x**: Group handling in obliterate, job scheduler template options for groups, global rate limit support

---

## Further Reading

| Resource | Type | Why Recommended |
|----------|------|-----------------|
| [BullMQ Documentation](https://docs.bullmq.io/) | Official Docs | Comprehensive reference for all features |
| [BullMQ Pro Docs](https://docs.bullmq.io/bullmq-pro/install) | Official Docs | Pro installation and feature guide |
| [BullMQ API Reference](https://api.docs.bullmq.io/) | API Docs | Complete class/method reference |
| [BullMQ GitHub](https://github.com/taskforcesh/bullmq) | Source Code | Issues, releases, and source |
| [Taskforce.sh Blog](https://blog.taskforce.sh/) | Tutorials | Rate limiting recipes, flows, mail services |
| [Rate Limiting Recipes](https://blog.taskforce.sh/rate-limit-recipes-in-nodejs-using-bullmq/) | Blog Post | 7 rate limiting strategies with code |
| [NestJS Queues Guide](https://docs.nestjs.com/techniques/queues) | Framework Docs | Official NestJS BullMQ integration |
| [Taskforce.sh Dashboard](https://taskforce.sh/) | Tool | Professional queue monitoring UI |
| [BullMQ Pro Edition Announcement](https://blog.taskforce.sh/bullmq-pro-edition/) | Blog Post | Pro motivation and roadmap |
| [Do Not Wait for Jobs](https://blog.taskforce.sh/do-not-wait-for-your-jobs-to-complete/) | Blog Post | Async patterns and event-driven design |

---

*This guide was synthesized from 42 sources. See `resources/bullmq-pro-sources.json` for full source list.*
