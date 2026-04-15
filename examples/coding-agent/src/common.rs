use serde::{Deserialize, Serialize};

// ── Constants ──

pub const DEFAULT_SERVER_URL: &str = "http://localhost:9090";
pub const DEFAULT_NAMESPACE: &str = "demo";
pub const DEFAULT_LANE: &str = "default";
pub const DEFAULT_MODEL: &str = "minimax/minimax-m2.7";

// ── Types ──

/// Input payload submitted with a coding task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPayload {
    pub issue: String,
    pub repo_context: String,
    pub language: String,
    pub max_turns: u32,
}

/// A single step in the agent's reasoning loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStep {
    pub turn: u32,
    pub thought: String,
    pub action: String,
    pub action_input: String,
    pub observation: String,
}

/// Result produced by the agent after completing (or failing) a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchResult {
    pub patch: String,
    pub steps_taken: u32,
    pub model_used: String,
    pub total_tokens: u64,
}

/// Payload for the human-in-the-loop review signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPayload {
    pub approved: bool,
    pub feedback: Option<String>,
}
