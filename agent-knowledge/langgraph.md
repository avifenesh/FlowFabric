# Learning Guide: LangGraph

**Generated**: 2026-04-09
**Sources**: 42 resources analyzed
**Depth**: deep
**Version Coverage**: LangGraph v1.1.6 (Python), @langchain/langgraph (JS/TS)

## Prerequisites

- Python 3.10+ or Node.js 18+ (for JS/TS SDK)
- Familiarity with LLM APIs (OpenAI, Anthropic, etc.)
- Basic understanding of directed graphs (nodes, edges, cycles)
- Working knowledge of LangChain primitives (chat models, tools) is helpful but not required
- An API key for at least one LLM provider

## TL;DR

- LangGraph is a **low-level orchestration framework** for building stateful, long-running AI agent workflows as directed graphs with nodes (computation) and edges (control flow).
- It uses a **StateGraph model** where shared state flows through the graph, updated by reducer functions at each node, inspired by Google's Pregel message-passing system.
- Core differentiators from vanilla LangChain agents: **durable execution** (checkpoint/resume), **human-in-the-loop** (interrupt/approve), **comprehensive memory** (short-term thread + long-term cross-thread), and **first-class streaming**.
- Two APIs: **Graph API** (declarative node/edge structure, visualizable) and **Functional API** (imperative Python with @entrypoint/@task decorators).
- Production-proven at Klarna, Uber, J.P. Morgan, Replit, Elastic, and 40+ other companies. MIT licensed with managed deployment via LangSmith Cloud.

---

## 1. What Is LangGraph

LangGraph is a low-level orchestration framework and runtime for building, managing, and deploying long-running, stateful agents. It focuses exclusively on **agent orchestration** -- how components work together -- rather than on prompt engineering or constraining architectural choices.

Key characteristics:
- **Graph-based**: Agents are modeled as directed graphs where nodes do work and edges determine control flow
- **Stateful**: A shared state object persists throughout execution and across invocations via checkpointing
- **Low-level by design**: Does not abstract away prompts or limit architecture; you have full control
- **Language support**: Python (primary) and JavaScript/TypeScript (via LangGraph.js)
- **MIT License**: Open-source core with optional managed platform

### LangGraph vs. Vanilla LangChain Agents

| Aspect | LangChain Agents | LangGraph |
|--------|-----------------|-----------|
| Abstraction level | High-level prebuilt architectures | Low-level graph primitives |
| Control flow | Implicit (AgentExecutor loop) | Explicit (nodes + edges) |
| State management | Limited to memory classes | Full StateGraph with reducers |
| Persistence | Not built-in | First-class checkpointing |
| Human-in-the-loop | Afterthought | Core primitive (interrupt) |
| Cycles and branching | Limited | Native support |
| Streaming | Basic | Seven streaming modes |
| Visualization | None | Built-in graph diagrams |

LangChain agents provide convenience for common LLM + tool-calling loops. LangGraph demands more explicit control but enables sophisticated orchestration patterns that simpler frameworks cannot support.

### The Agentic Spectrum

LangChain views agent capabilities on a spectrum:
- **Router**: LLM directs inputs to specific workflows
- **State Machine**: Multiple routing steps with loop-until-completion logic
- **Autonomous Agent**: Systems that build and retain tools for future use

LangGraph supports all levels, with increasing value as systems become more agentic.

---

## 2. Architecture and Execution Model

### Pregel-Inspired Runtime

LangGraph's runtime is built on **Pregel**, named after Google's Pregel algorithm for large-scale parallel graph computation. Execution proceeds in iterative **super-steps**, each containing three phases:

1. **Plan**: Determine which actors (PregelNodes) should execute based on channel subscriptions
2. **Execute**: Run all selected actors in parallel until completion, failure, or timeout. Channel updates remain invisible to other actors until the next step.
3. **Update**: Apply channel updates. Repeat until no actors remain scheduled or the recursion limit is reached.

Nodes begin inactive, become active upon receiving messages, and vote to halt when no incoming messages remain. Execution terminates when all nodes are inactive with no messages in transit.

### Core Components

```
StateGraph
  |
  +-- State (TypedDict / Pydantic / dataclass)
  |     +-- Channels (LastValue, Topic, BinaryOperatorAggregate)
  |     +-- Reducers (how updates merge)
  |
  +-- Nodes (Python/JS functions)
  |     +-- Receive state, config, runtime
  |     +-- Return state updates
  |
  +-- Edges (control flow)
  |     +-- Normal edges (add_edge)
  |     +-- Conditional edges (add_conditional_edges)
  |     +-- Send (dynamic fan-out)
  |     +-- Command (update + routing)
  |
  +-- Compilation (.compile())
        +-- Validates structure
        +-- Configures checkpointers, breakpoints
        +-- Returns executable CompiledStateGraph
```

---

## 3. StateGraph Model

### Defining State

State schemas can be defined using three approaches:

```python
# Option 1: TypedDict (recommended, fastest)
from typing import TypedDict, Annotated
from operator import add

class State(TypedDict):
    messages: Annotated[list, add_messages]
    step_count: int
    results: Annotated[list[str], add]

# Option 2: Pydantic BaseModel (enables validation, slower)
from pydantic import BaseModel

class State(BaseModel):
    messages: Annotated[list, add_messages]
    validated_field: str

# Option 3: dataclass (supports default values)
from dataclasses import dataclass, field

@dataclass
class State:
    messages: list = field(default_factory=list)
```

**TypeScript equivalent:**
```typescript
import { StateSchema, MessagesValue, ReducedValue } from "@langchain/langgraph";
import { z } from "zod";

const State = new StateSchema({
  messages: MessagesValue,
  count: z.number().default(0),
  allSteps: new ReducedValue(z.array(z.string()), {
    reducer: (current, newStep) => [...current, newStep],
  }),
});
```

### Reducers

Each state key has an independent reducer function governing how updates are applied. The default behavior overwrites values. Custom reducers enable accumulation:

```python
from langgraph.graph import add_messages

class State(TypedDict):
    # Default: overwrites on each update
    current_step: str
    
    # add_messages: appends messages, handles dedup by ID
    messages: Annotated[list[AnyMessage], add_messages]
    
    # operator.add: concatenates lists
    collected_data: Annotated[list[str], add]
```

The prebuilt `MessagesState` provides a convenient starting point:
```python
from langgraph.graph import MessagesState
# Equivalent to: messages: Annotated[list[AnyMessage], add_messages]
```

### Channel Types

- **LastValue**: Stores the most recent value (default for state keys)
- **Topic**: Configurable pub-sub for multiple values
- **BinaryOperatorAggregate**: Applies binary operators for aggregation across steps
- **EphemeralValue**: Temporary, non-persistent state
- **UntrackedValue** (JS): Transient state excluded from checkpoints

---

## 4. Nodes and Edges

### Nodes

Nodes are synchronous or asynchronous functions that receive state and return updates:

```python
from langgraph.types import Command

def my_node(state: State) -> dict:
    """Basic node returning state updates."""
    result = process(state["messages"])
    return {"messages": [result], "step_count": state["step_count"] + 1}

async def async_node(state: State, config: RunnableConfig) -> dict:
    """Async node with config access."""
    thread_id = config["configurable"]["thread_id"]
    result = await async_process(state)
    return {"results": [result]}

def routing_node(state: State) -> Command:
    """Node that combines updates with routing."""
    if state["needs_review"]:
        return Command(update={"status": "reviewing"}, goto="review")
    return Command(update={"status": "complete"}, goto=END)
```

Nodes are added via:
```python
builder = StateGraph(State)
builder.add_node("processor", my_node)
builder.add_node("reviewer", async_node)
```

### Edge Types

**Normal edges** -- direct routing:
```python
builder.add_edge(START, "processor")
builder.add_edge("processor", "reviewer")
builder.add_edge("reviewer", END)

# Shorthand for sequences:
builder.add_sequence(["processor", "reviewer", "finalizer"])
```

**Conditional edges** -- dynamic routing based on state:
```python
def should_continue(state: State) -> str:
    if state["messages"][-1].tool_calls:
        return "tools"
    return END

builder.add_conditional_edges("agent", should_continue)
```

**Send** -- dynamic fan-out for map-reduce patterns:
```python
from langgraph.types import Send

def fan_out(state: State):
    return [Send("process_item", {"item": item}) for item in state["items"]]

builder.add_conditional_edges("splitter", fan_out)
```

**Command** -- combined state update and routing (returned from nodes):
```python
def decision_node(state: State) -> Command:
    return Command(
        update={"decision": "approved"},
        goto="execute",
        graph=Command.PARENT  # target parent graph from subgraph
    )
```

### Special Nodes

- `START`: Entry point representing user input
- `END`: Terminal node marking workflow completion

---

## 5. Graph API vs. Functional API

LangGraph offers two complementary APIs:

### Graph API (Declarative)

```python
from langgraph.graph import StateGraph, START, END

builder = StateGraph(State)
builder.add_node("classify", classify_node)
builder.add_node("research", research_node)
builder.add_node("synthesize", synthesize_node)
builder.add_edge(START, "classify")
builder.add_conditional_edges("classify", route_by_category)
builder.add_edge("research", "synthesize")
builder.add_edge("synthesize", END)
graph = builder.compile(checkpointer=checkpointer)
```

**Best for**: Complex workflows requiring visualization, explicit state management with shared data across nodes, conditional branching, parallel execution paths, and team-based development.

### Functional API (Imperative)

```python
from langgraph.func import entrypoint, task
from langgraph.types import interrupt

@task
def research(topic: str) -> str:
    return llm.invoke(f"Research: {topic}")

@task
def synthesize(research: str) -> str:
    return llm.invoke(f"Synthesize: {research}")

@entrypoint(checkpointer=checkpointer)
def workflow(topic: str) -> dict:
    research_result = research(topic).result()
    approved = interrupt({"content": research_result, "action": "approve?"})
    if approved:
        return {"output": synthesize(research_result).result()}
    return {"output": "Rejected by user"}
```

**Best for**: Minimal code changes to existing procedural code, standard Python control flow (if/else, loops), rapid prototyping, function-scoped state.

### Comparison Table

| Aspect | Graph API | Functional API |
|--------|-----------|----------------|
| Control flow | Explicit graph structure | Standard Python (if/for/while) |
| State management | Shared state with reducers | Function-scoped variables |
| Checkpointing | New checkpoint per super-step | Saves task results to checkpoints |
| Visualization | Built-in diagrams | Not supported |
| Learning curve | Higher (graph concepts) | Lower (familiar Python) |
| Refactoring | Requires restructuring | Minimal changes |

Both APIs share the same runtime and support persistence, streaming, human-in-the-loop, and memory. They can coexist within the same application.

---

## 6. Checkpointing and Persistence

### How Checkpointing Works

LangGraph automatically captures graph state at each super-step boundary as **checkpoints** -- state snapshots organized into threads.

For a graph `START -> A -> B -> END`, four checkpoints are created:
1. Empty checkpoint with START as next node
2. Checkpoint after input, with node A queued
3. Checkpoint after A executes, with node B queued
4. Checkpoint after B executes, no remaining nodes

### Enabling Persistence

```python
from langgraph.checkpoint.memory import InMemorySaver

checkpointer = InMemorySaver()  # Development only
graph = builder.compile(checkpointer=checkpointer)

# Invoke with thread_id for persistence
config = {"configurable": {"thread_id": "user-123-session-1"}}
result = graph.invoke({"messages": [("user", "Hello")]}, config)
```

### Checkpointer Backends

| Backend | Package | Use Case |
|---------|---------|----------|
| InMemorySaver | langgraph-checkpoint | Development/testing |
| SqliteSaver | langgraph-checkpoint-sqlite | Local workflows |
| PostgresSaver | langgraph-checkpoint-postgres | Production |
| MongoDBSaver | langgraph-checkpoint-mongodb | Production |
| RedisSaver | langgraph-checkpoint-redis | Production |
| CosmosDBSaver | langgraph-checkpoint-cosmosdb | Azure environments |

All conform to the `BaseCheckpointSaver` interface with methods: `.put()`, `.put_writes()`, `.get_tuple()`, `.list()` and async variants.

### State Operations

```python
# Get current state
snapshot = graph.get_state(config)
# snapshot.values, snapshot.next, snapshot.config, snapshot.metadata

# Browse history
for state in graph.get_state_history(config):
    print(state.metadata["step"], state.next)

# Update state (creates new checkpoint)
graph.update_state(config, {"messages": [new_message]}, as_node="agent")
```

### Durability Modes

| Mode | Behavior | Trade-off |
|------|----------|-----------|
| `"exit"` | Persist only on completion/error | Best performance, no mid-execution recovery |
| `"async"` | Async persistence during next step | Balanced approach |
| `"sync"` | Synchronous checkpoint before continuing | Highest durability, performance cost |

### Serialization and Encryption

Default serializer is `JsonPlusSerializer` (ormsgpack + JSON). For unsupported types:

```python
# Pickle fallback for complex types (Pandas DataFrames, etc.)
serde = JsonPlusSerializer(pickle_fallback=True)

# AES encryption for sensitive state
from langgraph.checkpoint.serde.encrypted import EncryptedSerializer
serde = EncryptedSerializer.from_pycryptodome_aes()
# Reads LANGGRAPH_AES_KEY env var automatically on LangSmith
```

### Fault Tolerance

When a node fails mid-super-step, LangGraph stores **pending checkpoint writes** from successfully-completed nodes. Resuming from that super-step skips re-running successful nodes.

---

## 7. Human-in-the-Loop

### The interrupt() Primitive

```python
from langgraph.types import interrupt, Command

def approval_node(state: State) -> Command:
    decision = interrupt({
        "question": "Approve this action?",
        "details": state["action_details"]
    })
    if decision["approved"]:
        return Command(goto="execute")
    return Command(goto="cancel")
```

When `interrupt()` is called:
1. Graph state is saved via the persistence layer
2. Execution suspends indefinitely
3. The interrupt payload is returned to the caller
4. Resuming requires the same `thread_id` + `Command(resume=value)`

### Resume Pattern

```python
# Initial invocation hits interrupt
config = {"configurable": {"thread_id": "review-1"}}
result = graph.invoke({"request": "delete database"}, config)
# result contains interrupt info

# Human reviews and resumes
graph.invoke(Command(resume={"approved": True}), config)
```

### Common Patterns

**Tool call approval:**
```python
@tool
def send_email(to: str, subject: str, body: str):
    response = interrupt({"action": "send_email", "to": to, "message": "Approve?"})
    if response.get("action") == "approve":
        return actually_send_email(to=response.get("to", to), subject=subject, body=body)
```

**Validation loops:**
```python
def get_age_node(state: State):
    prompt = "What is your age?"
    while True:
        answer = interrupt(prompt)
        if isinstance(answer, int) and answer > 0:
            return {"age": answer}
        prompt = f"'{answer}' invalid. Enter a positive number."
```

**Static breakpoints (compile-time):**
```python
graph = builder.compile(
    checkpointer=checkpointer,
    interrupt_before=["dangerous_node"],
    interrupt_after=["review_node"]
)
```

### Critical Rules

1. **Never wrap interrupts in bare try/except** -- this catches the special exception that interrupts rely on
2. **Maintain consistent interrupt ordering** -- matching is index-based; reordering causes misalignment
3. **Use only JSON-serializable values** -- no functions or class instances
4. **Make side effects idempotent** -- code before interrupt re-executes on resume

---

## 8. Memory: Short-Term and Long-Term

### Short-Term Memory (Thread-Level)

Enabled automatically with any checkpointer. Each `thread_id` maintains its own conversation history:

```python
# Conversation 1
graph.invoke({"messages": [("user", "My name is Alice")]}, 
             {"configurable": {"thread_id": "t1"}})
# Later in same thread, agent remembers "Alice"
graph.invoke({"messages": [("user", "What's my name?")]},
             {"configurable": {"thread_id": "t1"}})
```

### Long-Term Memory (Cross-Thread)

The `Store` interface enables information retention across threads:

```python
from langgraph.store.memory import InMemoryStore
from langchain.embeddings import init_embeddings

# With semantic search
store = InMemoryStore(
    index={
        "embed": init_embeddings("openai:text-embedding-3-small"),
        "dims": 1536,
        "fields": ["food_preference", "$"]
    }
)

graph = builder.compile(checkpointer=checkpointer, store=store)
```

Accessing store in nodes:
```python
from langgraph.runtime import Runtime

async def call_model(state: MessagesState, runtime: Runtime[Context]):
    user_id = runtime.context.user_id
    memories = await runtime.store.asearch(
        (user_id, "memories"),
        query=state["messages"][-1].content,
        limit=3
    )
    # Use memories in prompt construction
```

### Memory Management for Long Conversations

- **Trim messages**: `trim_messages()` to keep within token limits
- **Delete messages**: `RemoveMessage` with unique IDs
- **Summarize**: Generate summaries replacing full history
- **Manage checkpoints**: View/delete thread history

---

## 9. Streaming

LangGraph supports seven streaming modes via `.stream()` (sync) and `.astream()` (async):

| Mode | Description |
|------|-------------|
| `values` | Complete state after each step |
| `updates` | Only changed keys from each node |
| `messages` | LLM tokens as (token, metadata) tuples |
| `custom` | User-defined data via `get_stream_writer()` |
| `checkpoints` | Checkpoint events (requires checkpointer) |
| `tasks` | Task start/finish events |
| `debug` | Comprehensive execution visibility |

### v2 Format (Recommended)

```python
async for chunk in graph.astream(
    inputs, config, stream_mode=["updates", "messages"], version="v2"
):
    if chunk["type"] == "messages":
        token, metadata = chunk["data"]
        print(f"[{metadata['langgraph_node']}] {token.content}", end="")
    elif chunk["type"] == "updates":
        print(f"Node completed: {chunk['data']}")
```

### Token-Level Streaming

Messages mode streams LLM output token-by-token with metadata including `langgraph_node` and `tags` fields for filtering.

### Subgraph Streaming

```python
async for chunk in graph.astream(inputs, subgraphs=True, version="v2"):
    print(chunk["ns"])  # () for root, ("subgraph_node:<task_id>",) for child
```

### Custom Streaming (Non-LangChain LLMs)

```python
from langgraph.config import get_stream_writer

def my_node(state: State):
    writer = get_stream_writer()
    for chunk in custom_llm_stream(state["messages"]):
        writer({"token": chunk})  # Emitted as "custom" stream events
    return {"messages": [full_response]}
```

### JS Streaming

```typescript
for await (const chunk of await graph.stream(inputs, {
  streamMode: ["updates", "messages"],
})) {
  const [mode, data] = chunk;
  console.log(mode, data);
}
```

JS adds a `tools` stream mode for tool lifecycle events (`on_tool_start`, `on_tool_event`, `on_tool_end`, `on_tool_error`).

---

## 10. Subgraphs

Subgraphs are compiled graphs used as nodes within parent graphs, enabling modular multi-agent architectures.

### Pattern 1: Subgraph Inside a Node (Different State Schemas)

```python
def call_subagent(state: ParentState):
    # Transform parent state to subgraph state
    result = subgraph.invoke({"agent_messages": state["messages"][-3:]})
    return {"messages": [result["final_answer"]]}

parent_builder.add_node("specialist", call_subagent)
```

### Pattern 2: Direct Subgraph as Node (Shared State)

```python
parent_builder.add_node("specialist", compiled_subgraph)
```

### Persistence Modes

| Mode | Config | Interrupts | Multi-turn | Use Case |
|------|--------|------------|------------|----------|
| Per-invocation | `checkpointer=None` | Yes | No | Independent requests |
| Per-thread | `checkpointer=True` | Yes | Yes | Conversational subagents |
| Stateless | `checkpointer=False` | No | No | Pure function calls |

### Key Design Principle

As long as the subgraph interface (input/output schemas) is respected, the parent graph can be built without knowing any subgraph implementation details.

---

## 11. Multi-Agent Patterns

### Pattern 1: Multi-Agent Collaboration (Shared Scratchpad)

All agents share a common message list. A simple router directs control based on tool calls or final answers.

```
START -> Router -> Agent A -> Router -> Agent B -> Router -> END
              (shared messages state)
```

### Pattern 2: Supervisor Agent

A supervisor LLM decides which specialized agent to invoke next. Each agent has its own scratchpad; only final responses go to the shared state.

```
START -> Supervisor -> [Research Agent | Code Agent | Writing Agent] -> Supervisor -> END
```

### Pattern 3: Hierarchical Agent Teams

Agents are themselves LangGraph objects (subgraphs). Supervisors can be nested, creating multi-level hierarchies.

```
Top Supervisor
  +-- Research Team (Supervisor + Agents)
  +-- Engineering Team (Supervisor + Agents)
```

### Pattern 4: Swarm / Handoff

Agents transfer control directly to each other using `Command(goto="other_agent")`, without a central supervisor.

### Benefits of Multi-Agent Architecture

- Specialized agents succeed better on focused tasks
- Individual prompts and fine-tuned LLMs per agent
- Independent testing, monitoring, and improvement of components
- Modular development across teams

---

## 12. Workflows vs. Agents

### Workflows (Predetermined Paths)

Common patterns:
- **Prompt chaining**: Sequential LLM calls processing previous output
- **Parallelization**: Independent subtasks running simultaneously
- **Routing**: Classifying inputs to specialized handlers
- **Orchestrator-worker**: Dynamic subtask delegation with synthesis
- **Evaluator-optimizer**: Iterative generation + assessment loops

### Agents (Dynamic Decision-Making)

Agents operate in continuous feedback loops where the LLM decides:
- Which tools to use
- When to stop
- How to recover from errors

### When to Use Each

| Criteria | Workflow | Agent |
|----------|----------|-------|
| Steps known in advance | Yes | No |
| Predictable execution | Yes | No |
| Needs autonomy | No | Yes |
| Complex tool selection | No | Yes |
| Reliability requirements | High | Variable |

---

## 13. Advanced Patterns

### Reflection / Self-Critique

```python
def generate_node(state):
    return {"draft": llm.invoke(state["prompt"])}

def critique_node(state):
    critique = llm.invoke(f"Critique this draft: {state['draft']}")
    return {"critique": critique}

def should_revise(state):
    if "satisfactory" in state["critique"].lower():
        return END
    return "generate"  # Loop back

builder.add_conditional_edges("critique", should_revise)
```

### Agentic RAG

```python
def route_query(state):
    if needs_retrieval(state["messages"][-1]):
        return "retrieve"
    return "respond"

def retrieve_node(state):
    docs = vectorstore.similarity_search(state["query"])
    return {"context": docs}

def grade_documents(state):
    if relevant(state["context"], state["query"]):
        return "generate"
    return "rewrite_query"
```

### Map-Reduce with Send

```python
def fan_out(state):
    return [Send("analyze", {"topic": t}) for t in state["topics"]]

def analyze(state):
    return {"analyses": [llm.invoke(f"Analyze: {state['topic']}")]}

def synthesize(state):
    combined = "\n".join(state["analyses"])
    return {"summary": llm.invoke(f"Synthesize: {combined}")}
```

### Error Handling Strategy

| Error Category | Handling | Example |
|----------------|----------|---------|
| Transient (network, rate limits) | RetryPolicy with backoff | API timeouts |
| LLM-recoverable (parsing) | Store error in state, loop back | Bad JSON output |
| User-fixable (missing info) | interrupt() for human input | Missing credentials |
| Unexpected | Let bubble up | Logic errors |

```python
from langgraph.pregel import RetryPolicy

builder.add_node(
    "api_call",
    api_node,
    retry=RetryPolicy(max_attempts=3, retry_on=ConnectionError)
)
```

### Node Caching

```python
from langgraph.types import CachePolicy

builder.add_node(
    "expensive_lookup",
    lookup_node,
    cache_policy=CachePolicy(ttl=300)  # Cache for 5 minutes
)
```

---

## 14. Time Travel

### Replay from Checkpoint

```python
# Get history
history = list(graph.get_state_history(config))
target = next(s for s in history if s.next == ("agent",))

# Replay -- nodes after checkpoint re-execute (LLM calls fire again)
graph.invoke(None, target.config)
```

### Fork with Modified State

```python
# Fork from a prior checkpoint with new state
graph.update_state(target.config, {"messages": [edited_message]}, as_node="user_input")
graph.invoke(None, target.config)
# Original history remains intact; this creates a new branch
```

Interrupts always re-trigger during time travel, enabling changing answers to earlier questions.

---

## 15. Observability with LangSmith

### Setup

```bash
export LANGSMITH_TRACING=true
export LANGSMITH_API_KEY=lsv2_...
```

### Capabilities

- **Traces**: Visualize execution from input to output with each step as a run
- **Selective tracing**: Use `tracing_context` for specific operations
- **Metadata and tags**: Custom annotations for filtering
- **Data protection**: Anonymizer feature masks sensitive patterns
- **Project organization**: Route logs to custom projects

---

## 16. Testing LangGraph Applications

### Unit Testing Nodes

```python
# Access individual nodes from compiled graph
result = graph.nodes["my_node"](test_state)
assert result["output"] == expected
```

### Integration Testing with Checkpoints

```python
from langgraph.checkpoint.memory import InMemorySaver

def test_approval_flow():
    checkpointer = InMemorySaver()
    graph = builder.compile(checkpointer=checkpointer)
    config = {"configurable": {"thread_id": "test-1"}}
    
    # Run until interrupt
    result = graph.invoke({"request": "action"}, config)
    assert "__interrupt__" in result
    
    # Resume with approval
    final = graph.invoke(Command(resume={"approved": True}), config)
    assert final["status"] == "completed"
```

### Best Practices

- Create fresh graphs/checkpointers per test for isolation
- Test individual nodes in isolation from graph structure
- Decompose large graphs into subgraphs for focused testing
- Use InMemorySaver for test efficiency

---

## 17. Deployment

### Project Structure

```
my-app/
  my_agent/
    utils/
      tools.py       # Tool definitions
      nodes.py       # Node functions
      state.py       # State definitions
    agent.py         # Graph construction
  .env               # Environment variables
  requirements.txt   # Dependencies
  langgraph.json     # Configuration
```

### langgraph.json Configuration

```json
{
  "dependencies": ["langchain_openai", "./my_agent"],
  "graphs": {
    "my_agent": "./my_agent/agent.py:graph"
  }
}
```

### Local Development Server

```bash
pip install -U "langgraph-cli[inmem]"
langgraph new my-app --template new-langgraph-project-python
langgraph dev
# API: http://127.0.0.1:2024
# Studio: https://smith.langchain.com/studio/?baseUrl=http://127.0.0.1:2024
```

### Deployment Options

| Option | Description | Best For |
|--------|-------------|----------|
| LangSmith Cloud | Managed, one-click from GitHub | Production without infra overhead |
| Control Plane | Hybrid/self-hosted | Enterprise with compliance needs |
| Standalone Server | Self-managed deployment | Full infrastructure control |

LangSmith Cloud provides: fault tolerance, horizontal scaling, Postgres checkpointing, double-texting handling, background jobs, cron scheduling, and Studio integration.

### LangGraph Cloud Features

- **Double-texting**: Four strategies (reject, queue, interrupt, rollback) for new inputs on running threads
- **Background jobs**: Long-running tasks with completion tracking via polling or webhooks
- **Cron scheduling**: Automated recurring tasks
- **Monitoring**: LangSmith integration for usage, errors, performance, and cost tracking

---

## 18. JavaScript/TypeScript SDK

### Installation

```bash
npm install @langchain/langgraph @langchain/core
```

### Key Differences from Python

| Feature | Python | JavaScript/TypeScript |
|---------|--------|----------------------|
| State schema | TypedDict / Pydantic | StateSchema + Zod |
| Type safety | Type hints | Full TypeScript generics |
| Reducers | Annotated[type, reducer] | ReducedValue class |
| Untracked state | Not available | UntrackedValue |
| Tool streaming | Not built-in | `tools` stream mode |
| Schema validation | Pydantic | Zod |
| Runtime | CPython / PyPy | Node.js / Deno / Browser |

### TypeScript Example

```typescript
import { StateGraph, START, END, StateSchema, MessagesValue } from "@langchain/langgraph";
import { z } from "zod";

const State = new StateSchema({
  messages: MessagesValue,
  step: z.string().default("init"),
});

const agent: typeof State.Node = async (state) => {
  const response = await model.invoke(state.messages);
  return { messages: [response], step: "complete" };
};

const graph = new StateGraph(State)
  .addNode("agent", agent)
  .addEdge(START, "agent")
  .addEdge("agent", END)
  .compile();
```

---

## 19. LangGraph Studio

LangGraph Studio is a free visual interface for building, testing, and debugging agents locally.

### Features

- **Visualization**: See each step including prompts, tool calls, results, and outputs
- **Interactive testing**: Test inputs, inspect intermediate states without deployment
- **Hot-reloading**: Code changes appear immediately without restart
- **Re-run from any step**: Test changes by replaying from specific checkpoints
- **Token/latency metrics**: Performance visibility per node

### Usage

```bash
langgraph dev  # Starts local server + Studio connection
# Open: https://smith.langchain.com/studio/?baseUrl=http://127.0.0.1:2024
```

### Data Privacy

Set `LANGSMITH_TRACING=false` to prevent data transmission to LangSmith servers.

---

## 20. Frontend Integration

### React Hook: useStream

```typescript
import { useStream } from "@langchain/langgraph-sdk/react";

function AgentChat() {
  const stream = useStream({
    apiUrl: "http://localhost:2024",
    assistantId: "my_agent",
  });

  // stream.values contains completed outputs per node
  // stream.messages contains streaming tokens
  // getMessagesMetadata maps tokens to originating nodes
}
```

### Agent Chat UI

Pre-built Next.js application supporting:
- Real-time chat messaging
- Automatic tool call rendering
- Interrupted thread handling
- Time-travel debugging
- State forking

Available at `agentchat.vercel.app` or self-hosted via npm.

---

## Common Pitfalls

| Pitfall | Why It Happens | How to Avoid |
|---------|---------------|--------------|
| Wrapping interrupt() in try/except | Catches the special pause exception | Never use bare try/except around interrupts |
| Non-idempotent code before interrupt | Code re-executes on resume | Place side effects after interrupt or in separate nodes |
| Forgetting thread_id in config | No state persistence between calls | Always pass configurable.thread_id |
| Oversized nodes | Failure restarts entire node | Decompose into smaller nodes for resilience |
| Storing formatted text in state | Couples state to presentation | Store raw data; format in node prompts |
| Not compiling the graph | Runtime errors | Always call .compile() before use |
| Infinite loops | No recursion limit set | Set config.recursion_limit (default varies) |
| Non-serializable interrupt values | Checkpointing fails | Use only JSON-serializable payloads |
| Parallel tool calls in per-thread subgraphs | Checkpoint namespace conflicts | Use per-invocation persistence for parallelism |

## Best Practices

1. **Store raw data in state, format on-demand in prompts** -- enables different nodes to format differently without schema changes
2. **Decompose nodes for resilience** -- each node should do one thing well; failures are isolated
3. **Use Command objects for routing** -- makes control flow explicit and traceable
4. **Prefer async nodes for I/O-bound work** -- use `async def` and `.ainvoke()` / `.astream()`
5. **Enable v2 streaming format** -- unified StreamPart structure across all modes
6. **Test nodes in isolation** -- access via `graph.nodes["name"]` for unit testing
7. **Use InMemorySaver for development, Postgres for production** -- never InMemorySaver in production
8. **Set recursion limits** -- prevent runaway agent loops
9. **Implement RetryPolicy for external calls** -- handle transient failures gracefully
10. **Visualize your graph** -- `graph.get_graph().draw_mermaid_png()` for debugging and documentation

---

## Production Case Studies

LangGraph is used by 40+ companies across diverse industries:

| Company | Industry | Use Case |
|---------|----------|----------|
| Klarna | Fintech | Customer support agents |
| Uber | Transportation | Code generation |
| J.P. Morgan | Finance | Domain-specific copilot |
| Replit | Developer tools | Code generation |
| Elastic | Search/Security | Security analysis |
| GitLab | DevOps | Code generation |
| LinkedIn | Social/Professional | Code generation |
| Cisco | Networking | Customer support + DevOps |
| BlackRock | Finance | Financial services copilot |
| City of Hope | Healthcare | Medical applications |

---

## Further Reading

| Resource | Type | Why Recommended |
|----------|------|-----------------|
| [LangGraph Docs](https://docs.langchain.com/oss/python/langgraph/overview) | Official Docs | Comprehensive reference, recently restructured |
| [LangGraph Quickstart](https://docs.langchain.com/oss/python/langgraph/quickstart) | Tutorial | Best starting point with working examples |
| [Thinking in LangGraph](https://docs.langchain.com/oss/python/langgraph/thinking-in-langgraph) | Conceptual | Mental model and design philosophy |
| [Graph API Guide](https://docs.langchain.com/oss/python/langgraph/graph-api) | Reference | Complete StateGraph/node/edge documentation |
| [Functional API Guide](https://docs.langchain.com/oss/python/langgraph/functional-api) | Reference | @entrypoint/@task patterns |
| [Persistence Deep Dive](https://docs.langchain.com/oss/python/langgraph/persistence) | Reference | Checkpointing, stores, serialization |
| [Streaming Guide](https://docs.langchain.com/oss/python/langgraph/streaming) | Reference | All seven streaming modes |
| [Human-in-the-Loop](https://docs.langchain.com/oss/python/langgraph/interrupts) | Reference | interrupt() patterns and rules |
| [Multi-Agent Blog](https://blog.langchain.com/langgraph-multi-agent-workflows) | Blog | Supervisor, collaboration, hierarchy patterns |
| [Reflection Agents Blog](https://blog.langchain.com/reflection-agents/) | Blog | Self-critique and LATS patterns |
| [LangGraph.js](https://github.com/langchain-ai/langgraphjs) | GitHub | TypeScript SDK and examples |
| [API Reference](https://reference.langchain.com/python/langgraph) | API Docs | Full class/function reference |
| [LangChain Academy](https://academy.langchain.com/courses/intro-to-langgraph) | Course | Free structured learning path |
| [SQL Agent Tutorial](https://docs.langchain.com/oss/python/langgraph/sql-agent) | Tutorial | End-to-end practical example |
| [Agentic RAG Guide](https://docs.langchain.com/oss/python/langgraph/agentic-rag) | Tutorial | Retrieval + agent patterns |
| [PyPI Package](https://pypi.org/project/langgraph/) | Package | Version history and metadata |

---

*This guide was synthesized from 42 sources. See `resources/langgraph-sources.json` for the full source list with quality scores.*
