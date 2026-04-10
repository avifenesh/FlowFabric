# Learning Guide: avifenesh/cairn - Personal Agent OS

**Generated**: 2026-04-09
**Sources**: 24 resources analyzed
**Depth**: medium

**Important note**: The repository `avifenesh/cairn-rs` does not exist. The actual project is `avifenesh/cairn`, a Go-based (not Rust) personal agent operating system. This guide covers the real project and clarifies the naming confusion.

## Prerequisites

- Familiarity with Go (1.25+) for backend development
- Understanding of LLM concepts (prompting, token budgets, tool use)
- Basic knowledge of SQLite and event-driven architectures
- Node.js/pnpm for frontend development (SvelteKit 5)
- Optional: familiarity with Valkey/Redis for the glide-mq integration

## TL;DR

- Cairn is a self-hosted, always-on personal agent OS distributed as a single Go binary - it monitors 12+ information sources (GitHub, Gmail, Calendar, HN, Reddit, npm, crates.io, RSS, etc.) and acts autonomously through an LLM-powered agent with 65+ tools.
- Created by Avi Fenesh (avifenesh), a System Software Engineer at AWS ElastiCache and maintainer of Valkey GLIDE.
- Three-layer agent architecture: Loop (60s tick) > Orchestrator (10min cycle with GATHER/DECIDE/EXECUTE) > ReAct agents with 6 subagent types.
- Three-tier memory: semantic (vector search with MMR), episodic (30-day decay), procedural (hot-reloadable SOUL.md identity).
- Integrates with Valkey via glide-mq sidecar for persistent inter-agent messaging, connecting it to the broader Valkey/GLIDE ecosystem.

## Why "cairn-rs" Does Not Exist

The repository `avifenesh/cairn-rs` returns a 404 on GitHub. There is no Rust version, Rust branch, or Rust rewrite of this project. The confusion may stem from:

1. **avifenesh/cairn** is the actual repository (Go-based)
2. **nickspiker/cairn** is an unrelated Rust crate for build-gated version control on crates.io
3. avifenesh does work extensively in Rust (speedkey, glide-mq NAPI bindings, Valkey GLIDE core), but cairn itself is pure Go

The remainder of this guide covers `avifenesh/cairn` as the intended subject.

## Core Concepts

### What Cairn Is

Cairn is a "Personal Agent OS" - a self-hosted system that monitors your digital life across multiple platforms and acts on your behalf through LLM-powered agents. It runs as a single Go binary with no external dependencies (no CGO, no Docker, no Node.js runtime required for the backend). The philosophy is "Models propose, humans dispose" - the system emphasizes human oversight at every critical juncture.

Key numbers: 65+ built-in tools, 12 signal sources, 6 subagent types, 7 dashboard pages, 90+ REST API routes, and a SvelteKit 5 frontend with 242 tests.

### Three-Layer Agent Architecture

The agent system operates at three distinct timescales:

**Layer 1: Loop (60s tick)** - A lightweight cron-style loop that checks for pending scheduled tasks and executes them. This is the heartbeat.

**Layer 2: Orchestrator (10-minute cycle)** - The brain runs a three-phase decision framework when idle:
- GATHER: A read-only ReAct agent queries 7 tools (goals, tasks, feeds, memory, status, journals, digests) and builds a FactSheet. The LLM cannot invent facts - it only reads pre-assembled data.
- DECIDE: A single LLM call receives the FactSheet plus ranked opportunities (scored by Impact x Confidence / Cost) and returns up to 5 structured actions, each citing facts.
- EXECUTE: Deterministic Go code runs the actions - no LLM calls during execution, preventing hallucination-driven drift.

**Layer 3: ReAct Agents** - The execution workhorses with mode-dependent limits:
- Talk mode: 40 rounds, read-only tool access
- Work mode: 80 rounds, operational tasks
- Coding mode: 400 rounds, full file/git operations

### Subagent Taxonomy

Six specialized agent types provide role-based execution:

| Type | Purpose | Constraints |
|------|---------|-------------|
| Planner | Task decomposition | Spawns workers/coders |
| Researcher | Web/memory search | Read-only access |
| Coder | File/git editing | Worktree isolation |
| Worker | Shell execution | Uses lighter model tier |
| Skill-Curator | Lifecycle management | Daily cron schedule |
| Project-Manager | Pipeline orchestration | Full tool access |

Nesting supports depth 3 by default (max 5), with foreground/background execution modes.

### Memory System

Three-tier memory addresses different retention needs:

**Semantic memory** stores facts, preferences, hard rules, decisions, and writing style. Each memory has scope (personal/project/global), status (proposed/accepted/rejected), and temporal bounds. Retrieval uses hybrid search: 0.3x keyword (SQLite FTS) + 0.7x vector (cosine distance) with MMR re-ranking for diversity and credibility weighting (source authority 0.8-1.1, corroboration boosts up to 1.2x).

**Episodic memory** captures timestamped events with 30-day exponential decay. Auto-extracted from conversations via a Mem0-style pipeline with deduplication at 0.92+ cosine similarity.

**Procedural memory (SOUL)** defines behavioral identity through a hot-reloadable SOUL.md file. Changes go through a patch workflow with human review gates.

Embeddings support three backends: NoopEmbedder (keyword-only fallback), OpenAIEmbedder, and BedrockEmbedder (AWS).

### Signal Plane

The signal plane monitors 12 sources with deduplication into SQLite:

| Category | Sources |
|----------|---------|
| Development | GitHub (REST+GraphQL), GitHub PRs (CI/review), Stack Overflow, Dev.to |
| Package Ecosystems | npm, crates.io |
| Social/News | Reddit, Hacker News (Firebase API), Twitter/X |
| Communication | Gmail, Google Calendar |
| Generic | RSS/Atom feeds |

Each poller implements a standard interface with incremental fetching (last-poll timestamps), failure backoff, bot detection, and deduplication via source+sourceID keys.

### Tool System

The tool system provides 65+ built-in tools with type-safe definitions via Go generics:

- **Built-in tools**: Organized across 35+ files covering file ops, shell, Git, web search, memory, scheduling, and integrations
- **Deferred tools**: 20 tools excluded from default LLM context but discoverable via `cairn.toolSearch` - loaded on demand to save tokens
- **Script tools**: Runtime-generated from JavaScript (goja), Starlark, or WASM scripts
- **User-created tools**: Persisted in SQLite with a LATM (Learn-Adapt-Teach-Maintain) lifecycle requiring human approval

Permission model: first-match ordered rules with tool name/glob patterns and allow/ask/deny actions.

### Event Bus

The typed async pub/sub backbone connects all modules:

```go
bus := eventbus.New(eventbus.WithQueueSize(4096))
unsub := eventbus.Subscribe[TaskCreated](bus, func(e TaskCreated) { ... })
eventbus.PublishAsync(bus, TaskCreated{...})
```

14 event categories span signals, LLM streaming, tasks, memory, identity, subagents, sessions, MCP, rules, skills, quality, payments, models, PRs, and system shutdown.

### Communication Channels

Multi-platform support with session continuity:

- **Telegram**: Full commands, inline keyboards, voice (Whisper STT + edge-tts TTS), forum topics, Telegram Stars payments
- **Discord**: Text messaging, slash commands, button interactions
- **Slack**: Socket Mode, block kit, bot filtering

Priority-based routing: Critical broadcasts everywhere; High goes to preferred channel; Medium suppressed during quiet hours; Low queued for digest.

### Server and Protocols

- 90+ REST routes with rate limiting
- SSE broadcaster with 1000-event ring buffer for reconnection recovery
- WebAuthn biometric authentication
- MCP server exposing tools to external agents (stdio + HTTP/SSE transports)
- MCP client discovering external servers and registering tools as `mcp.{server}.{tool}`

## Valkey/GLIDE Ecosystem Connection

### Creator's Role

Avi Fenesh (avifenesh) is a System Software Engineer at AWS ElastiCache and a maintainer of Valkey GLIDE - the official multi-language client for Valkey. He also created and maintains the glide-mq message queue library built on Valkey Streams.

### glide-mq Integration in Cairn

Cairn optionally integrates with glide-mq as a sidecar service for persistent inter-agent messaging:

```go
if cfg.GlideMQEnabled {
    glideMQClient = glidemq.NewClient(cfg.GlideMQURL, logger)
    glideMQClient.StartHealthProbe(shutdownCtx)
    agent.GlobalMailbox.SetGlideMQ(glideMQClient, logger)
}
```

The integration provides "Valkey-backed mailbox (inter-agent messages survive restart)." The mailbox uses a dual-storage approach:
- Primary: Valkey via glide-mq sidecar (3-second timeout)
- Fallback: In-memory inbox (50 messages per recipient)
- Read: Drains both sources, merges chronologically

The glide-mq client (`internal/glidemq/client.go`) communicates via HTTP to the sidecar, supporting job addition, queue counts, health probes, and graceful degradation.

### glide-mq Itself

glide-mq is a high-performance message queue for Node.js with first-class AI orchestration, built on Valkey/Redis Streams with Rust NAPI bindings. Key design: single round-trip per job (1 RTT) via Valkey Server Functions (FCALL). Benchmarks show 18-19K jobs/sec on AWS ElastiCache Valkey 8.2.

### Package Monitoring

Cairn's signal plane monitors both npm and crates.io registries, making it useful for tracking the Valkey ecosystem's package releases.

## Technology Stack

| Component | Technology |
|-----------|-----------|
| Backend | Go 1.25, no CGO |
| Database | SQLite via modernc.org/sqlite (WAL mode), optional PostgreSQL + pgvector |
| Cache/Queue | Valkey via glide-mq sidecar (optional) |
| Frontend | SvelteKit 5, Svelte 5 runes, Tailwind v4, shadcn-svelte |
| LLM Brain | Claude Opus 4.6 |
| LLM Mid | Claude Sonnet 4.6 |
| LLM Light | Claude Haiku 4.5 |
| LLM Providers | GLM (Z.ai), OpenAI-compatible, vLLM, Ollama, OpenRouter, AWS Bedrock |
| Embeddings | AWS Bedrock Titan, OpenAI-compatible |
| Voice | Whisper STT, edge-tts TTS |
| Protocol | MCP via mcp-go |
| Messaging | Telegram (telego), Discord (discordgo), Slack (slack-go) |
| Scripting | JavaScript (goja), Starlark, WASM (extism) |
| Sandbox | Linux namespaces, seccomp-bpf, Landlock LSM |

## LLM Provider System

The LLM client implements multi-provider streaming with resilience:

- **Fallback chains** (top-down): Brain > Mid > Light tier progression
- **Escalation chains** (bottom-up): Light > Mid > Brain for quality gates
- **Retry with backoff**: Exponential backoff with jitter, circuit breaker during cooldown
- **Budget management**: Thread-safe daily/weekly cost limits per model with CanAfford() pre-flight checks
- **5 event types**: TextDelta, ReasoningDelta, ToolCallDelta, MessageEnd, StreamError

## Dashboard Pages

The SvelteKit 5 frontend provides 7 primary routes:

| Route | Purpose |
|-------|---------|
| /today | Action hub - approvals, patches, memories digest |
| /feed | Signal stream - infinite scroll, source filters |
| /chat | AI conversation - streaming, multimodal (text, voice, files, vision) |
| /ops | Operations - approvals, tasks, activity, health |
| /brain | Knowledge - memories, identity editors (SOUL/USER/AGENTS/MEMORY.md) |
| /config | System - skills, agents, rules, crons |
| /settings | Preferences, auth, integrations |

16 reactive stores manage state. Keyboard shortcuts (Cmd+K palette, a/d approve/deny, j/k navigation), optimistic mutations, and a 50-entry offline write queue provide responsive UX.

## Plugin and Skill Ecosystem

Skills follow a modular format with metadata-driven loading:

- **5-directory discovery hierarchy**: project-local (`./skills`) through user global (`~/.cairn/skills`)
- **ClawHub marketplace**: ZIP-based distribution with integrity validation
- **GitHub plugin sources**: Trees API with 5-minute caching, meta-plugin aggregation
- **Security**: LLM-assisted review on installation, `DisableModel: true` flag for side-effect skills requiring approval
- **Hooks**: AgentHooks, PostTurnHooks, ToolHooks, LLMHooks - sequential execution in registration order
- **5 built-in plugins**: logging, budget, quality, skill_hooks, magic_docs

## Quick Start

```bash
# Build from source
git clone https://github.com/avifenesh/cairn
cd cairn
make build

# Configure
export LLM_API_KEY=your-key

# CLI mode
./cairn chat "What's on my GitHub?"

# Server mode (with optional frontend)
cd frontend && pnpm install && pnpm build && cd ..
make build-prod
./cairn serve  # Port 8787

# Optional: install skills
./cairn install skill https://github.com/user/skill.git
```

Required: `LLM_API_KEY` (or `GLM_API_KEY`/`OPENAI_API_KEY`). Everything else has sensible defaults.

## Project Structure

```
cmd/cairn/           CLI entry point
internal/
  agent/             Orchestration, ReAct, subagents, compaction
  agentmsg/          Agent messaging
  agenttype/         Agent type definitions
  auth/              WebAuthn authentication
  channel/           Telegram, Discord, Slack adapters
  config/            Configuration management
  cron/              Scheduled tasks
  db/                SQLite/PostgreSQL operations
  eventbus/          Typed async pub/sub
  glidemq/           Glide-MQ sidecar client
  knowledge/         Knowledge management
  llm/               Multi-provider LLM client
  mcp/               MCP server + client
  memory/            Semantic/episodic/procedural memory
  plugin/            Plugin system
  prompts/           Prompt management
  rules/             Automation engine
  sandbox/           OS-level isolation
  signal/            12 source pollers
  skill/             Skill lifecycle
  task/              Task engine with lease-based scheduling
  tool/              65+ built-in tools
  voice/             STT/TTS integration
  worktree/          Git worktree isolation
frontend/            SvelteKit 5 dashboard
sidecar/             Node.js sidecar (Twitter scraping)
skills/              17 bundled SKILL.md files
docs/design/         11 architecture documents
```

## Common Pitfalls

| Pitfall | Why It Happens | How to Avoid |
|---------|---------------|--------------|
| Confusing with nickspiker/cairn | Same name on crates.io | avifenesh/cairn is Go, nickspiker/cairn is Rust |
| No releases yet | Project is pre-release | Build from source on main branch |
| LLM costs accumulate | Always-on orchestrator every 10min | Set BUDGET_DAILY_CAP, use lighter models for workers |
| glide-mq sidecar optional | Many assume Valkey is required | Cairn works fully with SQLite + in-memory fallback |
| Frontend requires separate build | Not embedded by default in dev | Use make build-prod for production binary with embedded frontend |

## Best Practices

1. Start with `LLM_API_KEY` only and add integrations incrementally (Source: README)
2. Use SOUL.md to define agent personality and behavioral constraints (Source: VISION.md)
3. Set budget caps before enabling idle mode to prevent cost runaway (Source: VISION.md)
4. Enable glide-mq sidecar only when multi-agent messaging persistence is needed (Source: main.go)
5. Use approval gates for irreversible actions (merge PRs, send emails, push to main) (Source: task-engine design)
6. Keep skills in the local ./skills directory for fastest iteration (Source: plugin-skills design)

## Design Differentiators

1. **Anti-hallucination architecture**: GATHER/DECIDE/EXECUTE separation means the LLM never sees live data and execution never involves the LLM
2. **Single binary**: No Docker, no Node.js, no Python runtime required (pure Go + modernc.org SQLite)
3. **Human-in-the-loop by default**: Approval gates, soul patch review, proposed/accepted memory states
4. **Self-improving prompts**: DSPy-style ExemplarStore + PromptRefiner with owner approval
5. **Graceful degradation**: glide-mq unavailable? Falls back to in-memory. Embedder down? Falls back to keyword search.

## Further Reading

| Resource | Type | Why Recommended |
|----------|------|-----------------|
| [avifenesh/cairn](https://github.com/avifenesh/cairn) | Repository | Primary source, README, and code |
| [VISION.md](https://github.com/avifenesh/cairn/blob/main/docs/design/VISION.md) | Design doc | Architecture principles and differentiators |
| [Event Bus Design](https://github.com/avifenesh/cairn/blob/main/docs/design/pieces/01-event-bus.md) | Design doc | Typed pub/sub backbone |
| [Agent Core Design](https://github.com/avifenesh/cairn/blob/main/docs/design/pieces/04-agent-core.md) | Design doc | ReAct loop, orchestrator, subagents |
| [Memory System Design](https://github.com/avifenesh/cairn/blob/main/docs/design/pieces/06-memory-system.md) | Design doc | Three-tier memory architecture |
| [Tool System Design](https://github.com/avifenesh/cairn/blob/main/docs/design/pieces/03-tool-system.md) | Design doc | 65+ tools, registry, permissions |
| [Task Engine Design](https://github.com/avifenesh/cairn/blob/main/docs/design/pieces/05-task-engine.md) | Design doc | Scheduling, leases, approvals |
| [Signal Plane Design](https://github.com/avifenesh/cairn/blob/main/docs/design/pieces/07-signal-plane.md) | Design doc | 12-source polling system |
| [Plugin/Skills Design](https://github.com/avifenesh/cairn/blob/main/docs/design/pieces/08-plugin-skills.md) | Design doc | Skill format, marketplace, security |
| [Server Protocols Design](https://github.com/avifenesh/cairn/blob/main/docs/design/pieces/09-server-protocols.md) | Design doc | REST API, SSE, MCP, WebAuthn |
| [Frontend Design](https://github.com/avifenesh/cairn/blob/main/docs/design/pieces/10-frontend.md) | Design doc | SvelteKit 5 dashboard architecture |
| [Channel Adapters Design](https://github.com/avifenesh/cairn/blob/main/docs/design/pieces/11-channel-adapters.md) | Design doc | Telegram/Discord/Slack integration |
| [avifenesh/glide-mq](https://github.com/avifenesh/glide-mq) | Repository | Message queue that Cairn uses as sidecar |
| [avifenesh/speedkey](https://github.com/avifenesh/speedkey) | Repository | Rust NAPI Valkey client powering glide-mq |
| [CONTRIBUTING.md](https://github.com/avifenesh/cairn/blob/main/CONTRIBUTING.md) | Guide | Development setup and workflow |
| [avifenesh profile](https://github.com/avifenesh) | Profile | Creator's background and ecosystem context |

---

*This guide was synthesized from 24 sources. See `resources/cairn-rs-sources.json` for full source list with quality scores.*
