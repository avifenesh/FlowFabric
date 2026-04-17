//! Shared types for the media-pipeline example.
//!
//! The pipeline is `transcribe → summarize → embed`. Each stage produces a
//! typed result that the next stage consumes. `ApprovalDecision` is the
//! payload carried by the human-review signal delivered to the summarize
//! waitpoint.
//!
//! Names are canonical across all three workers + both CLIs. Do not
//! introduce synonyms (`text` vs `transcript`, etc.) — one source of truth.

use serde::{Deserialize, Serialize};

/// Initial pipeline input — supplied by the submit CLI.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PipelineInput {
    pub audio_path: String,
    pub title: Option<String>,
}

/// Output of the transcribe stage (W1 / whisper.cpp).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TranscribeResult {
    pub transcript: String,
    pub duration_ms: u64,
}

/// Output of the summarize stage (W3 / llama.cpp).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SummarizeResult {
    pub summary: String,
    pub token_count: u32,
}

/// Output of the embed stage (W3 / minilm).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EmbedResult {
    pub vector: Vec<f32>,
    pub dim: u32,
}

/// Human-review decision delivered via HMAC-signed signal to the summarize
/// waitpoint. `snake_case` serde so the wire form is `{"approve": {}}` /
/// `{"reject": {"reason": "…"}}`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Reject { reason: String },
}

/// Signal name expected by the summarize worker's resume condition.
pub const SIGNAL_NAME_APPROVAL: &str = "summary_approved";

/// Execution kinds — informational; scheduling is by capability.
pub const KIND_TRANSCRIBE: &str = "media.transcribe";
pub const KIND_SUMMARIZE: &str = "media.summarize";
pub const KIND_EMBED: &str = "media.embed";

/// Prefix labels for multiplexed tail output in the submit CLI.
pub const LABEL_TRANSCRIBE: &str = "transcribe";
pub const LABEL_SUMMARIZE: &str = "summarize";
pub const LABEL_EMBED: &str = "embed";
