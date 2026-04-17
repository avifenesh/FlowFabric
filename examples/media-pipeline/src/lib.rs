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

/// Field name used by `ff_append_frame` (lua/stream.lua:112) for the frame
/// body bytes. Single source of truth so summarize/submit/review don't
/// drift if RFC-006 ever renames the field.
pub const FRAME_FIELD_PAYLOAD: &str = "payload";

/// Field name used by `ff_append_frame` for the frame-type tag
/// (lua/stream.lua:110). See [`FRAME_FIELD_PAYLOAD`].
pub const FRAME_FIELD_TYPE: &str = "frame_type";

/// Soft cap on summary length shared between summarize (pre-persist) and
/// embed (pre-vectorize). MiniLM-L6-v2 truncates to 256 tokens (~1000
/// chars) anyway; 4096 is a belt-and-suspenders upper bound on
/// lease-healthy embed time.
pub const SUMMARY_CHAR_CAP: usize = 4096;

/// Execution kinds — informational; scheduling is by capability.
pub const KIND_TRANSCRIBE: &str = "media.transcribe";
pub const KIND_SUMMARIZE: &str = "media.summarize";
pub const KIND_EMBED: &str = "media.embed";

/// Prefix labels for multiplexed tail output in the submit CLI.
pub const LABEL_TRANSCRIBE: &str = "transcribe";
pub const LABEL_SUMMARIZE: &str = "summarize";
pub const LABEL_EMBED: &str = "embed";

/// Frame type summarize.rs appends with the final `SummarizeResult`
/// JSON so any worker that re-claims after approval can resume without
/// re-running the LLM (RFC-004 "any worker can resume" invariant).
pub const FRAME_SUMMARY_FINAL: &str = "summary_final";

/// Walk a frame sequence from the tail looking for the LAST
/// `summary_final` frame and deserialize its payload.
///
/// Hoisted out of summarize.rs so it can be unit-tested: PR#8 Gemini
/// review caught that the previous implementation capped the read at
/// 500 frames, so a long generation could bury the trailing
/// `summary_final` past the window and resume would silently regenerate.
/// The fix is to read up to `STREAM_READ_HARD_CAP` (== the
/// append_frame retention_maxlen), but we also add a regression test
/// here that constructs a frame sequence with 600+ entries + a
/// trailing `summary_final` and confirms this helper still finds it.
///
/// `frames` is expected in append order (XRANGE `-` to `+`). The
/// reverse walk makes the hot path O(frames-since-last-summary_final)
/// and guarantees we prefer the newest entry on the unlikely chance
/// multiple `summary_final` frames exist in the same attempt stream.
///
/// Returns `Ok(None)` when no matching frame is present — the caller
/// should treat this as "fresh claim" and drive generation.
pub fn find_last_summary_final<'a, F>(
    frames: impl IntoIterator<Item = &'a F>,
) -> Result<Option<SummarizeResult>, serde_json::Error>
where
    F: FrameView + 'a,
{
    let mut last: Option<&'a F> = None;
    for f in frames {
        if f.frame_type() == FRAME_SUMMARY_FINAL {
            last = Some(f);
        }
    }
    match last {
        Some(f) => Ok(Some(serde_json::from_str(f.payload())?)),
        None => Ok(None),
    }
}

/// Minimal view of a stream frame used by [`find_last_summary_final`].
/// Both the production call site (which passes `ff_core::contracts::StreamFrame`)
/// and the test harness (which passes a bare struct) can implement this
/// without pulling ff-core into the example's public API.
pub trait FrameView {
    fn frame_type(&self) -> &str;
    fn payload(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Harness frame — same shape as ff_core::contracts::StreamFrame but
    /// lets the test skip the full BTreeMap dance.
    struct TestFrame {
        frame_type: String,
        payload: String,
    }

    impl FrameView for TestFrame {
        fn frame_type(&self) -> &str {
            &self.frame_type
        }
        fn payload(&self) -> &str {
            &self.payload
        }
    }

    fn token_frame(i: usize) -> TestFrame {
        TestFrame {
            frame_type: "summary_token".to_owned(),
            payload: format!("tok{i}"),
        }
    }

    fn summary_final_frame(summary: &str, tokens: u32) -> TestFrame {
        let body = serde_json::to_string(&SummarizeResult {
            summary: summary.to_owned(),
            token_count: tokens,
        })
        .unwrap();
        TestFrame {
            frame_type: FRAME_SUMMARY_FINAL.to_owned(),
            payload: body,
        }
    }

    /// PR#8 regression: previously the caller capped the read at 500
    /// frames, so a generation with >500 `summary_token` entries
    /// followed by a single trailing `summary_final` would silently
    /// return `None` and trigger a full LLM re-run on resume.
    ///
    /// We construct 700 token frames + 1 summary_final at the tail and
    /// confirm the helper still finds it. 700 is comfortably past the
    /// old 500 limit but well under `STREAM_READ_HARD_CAP == 10_000`.
    #[test]
    fn finds_summary_final_past_old_500_limit() {
        let mut frames: Vec<TestFrame> = (0..700).map(token_frame).collect();
        frames.push(summary_final_frame("the real summary", 700));

        let got = find_last_summary_final(&frames).expect("decode");
        let got = got.expect("summary_final must be found past index 500");
        assert_eq!(got.summary, "the real summary");
        assert_eq!(got.token_count, 700);
    }

    /// Multiple `summary_final` frames (can happen if an earlier
    /// attempt persisted then the worker retried mid-suspend and
    /// re-appended): take the LAST one so we reflect the most recent
    /// view of the work.
    #[test]
    fn prefers_last_summary_final_when_multiple() {
        let frames = vec![
            summary_final_frame("stale", 10),
            token_frame(0),
            summary_final_frame("fresh", 42),
        ];
        let got = find_last_summary_final(&frames)
            .expect("decode")
            .expect("found");
        assert_eq!(got.summary, "fresh");
        assert_eq!(got.token_count, 42);
    }

    /// No `summary_final` present → `Ok(None)`, caller must drive
    /// generation rather than trying to decode the empty case.
    #[test]
    fn returns_none_when_absent() {
        let frames: Vec<TestFrame> =
            (0..50).map(token_frame).collect();
        let got = find_last_summary_final(&frames).expect("decode");
        assert!(got.is_none(), "expected None, got {got:?}");
    }

    /// A corrupt `summary_final` payload (non-JSON) should bubble up
    /// as a deserialization error, not be silently ignored — that
    /// would mask a bug in the writer.
    #[test]
    fn propagates_decode_error_on_corrupt_payload() {
        let frames = vec![TestFrame {
            frame_type: FRAME_SUMMARY_FINAL.to_owned(),
            payload: "not-json-at-all".to_owned(),
        }];
        let err = find_last_summary_final(&frames)
            .expect_err("expected decode error");
        // Match on the serde_json error category — any parse error is fine.
        assert!(err.is_syntax() || err.is_data());
    }
}
