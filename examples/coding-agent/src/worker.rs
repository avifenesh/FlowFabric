mod common;
mod llm;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use ff_core::types::BudgetId;
use ff_sdk::{
    ConditionMatcher, FlowFabricAdminClient, FlowFabricWorker, SuspendOutcome,
    TimeoutBehavior, WorkerConfig,
};
use tokio::sync::Mutex;

use common::{AgentStep, PatchResult, TaskPayload, DEFAULT_LANE, DEFAULT_MODEL, DEFAULT_NAMESPACE};
use llm::{LlmClient, Message, Usage};

#[derive(Parser)]
#[command(name = "coding-agent-worker", about = "FlowFabric coding agent worker")]
struct Args {
    /// Valkey host.
    #[arg(long, env = "FF_HOST", default_value = "localhost")]
    host: String,

    /// Valkey port.
    #[arg(long, env = "FF_PORT", default_value_t = 6379)]
    port: u16,

    /// ff-server base URL. The worker claims via
    /// `POST /v1/workers/{id}/claim` — the scheduler-routed entry
    /// point that runs admission control server-side.
    #[arg(long, env = "FF_SERVER_URL", default_value = "http://localhost:9090")]
    server_url: String,

    /// Optional bearer token for ff-server (matches FF_API_TOKEN on
    /// the server side). Leave unset for an unauthenticated dev server.
    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,

    /// OpenRouter API key.
    #[arg(long, env = "OPENROUTER_API_KEY")]
    api_key: String,

    /// LLM model name.
    #[arg(long, env = "OPENROUTER_MODEL", default_value = DEFAULT_MODEL)]
    model: String,

    /// FlowFabric namespace.
    #[arg(long, default_value = DEFAULT_NAMESPACE)]
    namespace: String,

    /// FlowFabric lane.
    #[arg(long, default_value = DEFAULT_LANE)]
    lane: String,

    /// Budget ID for token tracking. If set, reports usage after each LLM turn.
    #[arg(long, env = "FF_BUDGET_ID")]
    budget_id: Option<String>,
}

const SYSTEM_PROMPT: &str = "\
You are a coding agent. Given a coding issue, solve it step by step.
Respond in this exact format:
THOUGHT: <your reasoning>
ACTION: <edit|search|view|submit>
ACTION_INPUT: <content for the action>

When ready with final solution, use ACTION: submit with the complete code.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "coding_agent=info,ff_sdk=info".into()),
        )
        .init();

    let args = Args::parse();

    let config = WorkerConfig::new(
        &args.host,
        args.port,
        "coding-agent",
        format!("coding-agent-{}", uuid::Uuid::new_v4()),
        &args.namespace,
        &args.lane,
    );

    let worker = FlowFabricWorker::connect(config).await?;
    let admin = match args.api_token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(tok) => FlowFabricAdminClient::with_token(&args.server_url, tok)?,
        None => FlowFabricAdminClient::new(&args.server_url)?,
    };
    let llm = LlmClient::new(&args.api_key, &args.model);

    let lane = ff_core::types::LaneId::try_new(&args.lane)?;

    // Patches awaiting review. suspend() consumes the task, so we stash the
    // result here. When the execution resumes and the worker re-claims it,
    // we look it up and complete immediately.
    let pending: Arc<Mutex<HashMap<String, PatchResult>>> = Default::default();

    let budget_id = args.budget_id.as_ref().map(|id| {
        BudgetId::parse(id).unwrap_or_else(|e| {
            eprintln!("invalid --budget-id: {e}");
            std::process::exit(1);
        })
    });

    tracing::info!(
        model = %args.model,
        budget_id = budget_id.as_ref().map(|b| b.to_string()).as_deref(),
        "coding-agent worker started"
    );

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received");
                break;
            }
            // 10s grant TTL gives the worker time to call
            // claim_from_grant even under moderate network jitter.
            result = worker.claim_via_server(&admin, &lane, 10_000) => {
                match result {
                    Ok(Some(task)) => {
                        let eid = task.execution_id().to_string();

                        // Re-claim after human review. resume_signals() returns the
                        // matched signal for this resumed attempt (empty Vec on fresh
                        // claims). We branch on the ReviewPayload inside the signal
                        // rather than assuming approval — the approve CLI now
                        // delivers either approve or reject via the same signal name,
                        // so the worker MUST inspect payload.approved to decide
                        // complete-vs-fail. Without this check, a rejection would
                        // incorrectly complete the execution with the old patch.
                        let signals = match task.resume_signals().await {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!(execution_id = %eid, error = %e, "resume_signals failed");
                                continue;
                            }
                        };
                        if let Some(sig) = signals
                            .iter()
                            .find(|s| s.signal_name == "review_response")
                        {
                            let decision: Option<common::ReviewPayload> = sig
                                .payload
                                .as_deref()
                                .and_then(|b| serde_json::from_slice(b).ok());
                            let patch_result = pending.lock().await.remove(&eid);
                            match decision {
                                Some(d) if d.approved => {
                                    // Approval: complete with the stashed patch if we
                                    // still have it. If the stash is gone (worker
                                    // restarted between suspend and resume), fall
                                    // through to `process_task` below so the agent
                                    // re-runs and re-suspends — reviewer will see a
                                    // fresh prompt. An approval MUST NOT be treated
                                    // as rejection just because local state was lost
                                    // (caught by @cursor-bugbot + @gemini-code-assist
                                    // on PR #59).
                                    match patch_result {
                                        Some(patch) => {
                                            tracing::info!(execution_id = %eid, "review approved — completing");
                                            if let Err(e) = task.complete(Some(serde_json::to_vec(&patch)?)).await {
                                                tracing::error!(execution_id = %eid, error = %e, "complete failed");
                                            }
                                            continue;
                                        }
                                        None => {
                                            tracing::warn!(
                                                execution_id = %eid,
                                                "review approved but patch stash missing (likely worker restart) — re-running agent"
                                            );
                                            if let Err(e) = process_task(task, &llm, &args.model, budget_id.as_ref(), &pending).await {
                                                tracing::error!(execution_id = %eid, error = %e, "task re-run failed");
                                            }
                                            continue;
                                        }
                                    }
                                }
                                Some(d) => {
                                    let reason = d
                                        .feedback
                                        .unwrap_or_else(|| "rejected by reviewer".to_owned());
                                    tracing::info!(
                                        execution_id = %eid,
                                        reason = %reason,
                                        "review rejected — failing with reviewer feedback"
                                    );
                                    if let Err(e) = task.fail(&reason, "human_rejected").await {
                                        tracing::error!(execution_id = %eid, error = %e, "fail call failed");
                                    }
                                    continue;
                                }
                                None => {
                                    // Signal with no/invalid payload — defensive fail
                                    // so a malformed approve CLI doesn't silently
                                    // complete with stale stash.
                                    tracing::error!(
                                        execution_id = %eid,
                                        "review_response signal had no parseable ReviewPayload; failing"
                                    );
                                    if let Err(e) = task
                                        .fail("review signal had unparseable payload", "bad_signal")
                                        .await
                                    {
                                        tracing::error!(execution_id = %eid, error = %e, "fail call failed");
                                    }
                                    continue;
                                }
                            }
                        }

                        if let Err(e) = process_task(task, &llm, &args.model, budget_id.as_ref(), &pending).await {
                            tracing::error!(execution_id = %eid, error = %e, "task processing failed");
                        }
                    }
                    Ok(None) => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "claim_via_server failed");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn process_task(
    task: ff_sdk::ClaimedTask,
    llm: &LlmClient,
    model_name: &str,
    budget_id: Option<&BudgetId>,
    pending: &Arc<Mutex<HashMap<String, PatchResult>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let eid = task.execution_id().to_string();

    let payload: TaskPayload = serde_json::from_slice(task.input_payload())?;
    tracing::info!(
        execution_id = %eid,
        language = %payload.language,
        max_turns = payload.max_turns,
        "processing coding task"
    );

    task.update_progress(5, "starting agent loop").await?;

    let max_turns = if payload.max_turns == 0 { 10 } else { payload.max_turns };

    let mut messages = vec![
        Message {
            role: "system".into(),
            content: SYSTEM_PROMPT.into(),
        },
        Message {
            role: "user".into(),
            content: format!(
                "Language: {}\n\nIssue:\n{}\n\nRepository context:\n{}",
                payload.language, payload.issue, payload.repo_context,
            ),
        },
    ];

    let mut total_usage = Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    };
    let mut final_patch: Option<String> = None;
    let mut steps_taken: u32 = 0;

    for turn in 1..=max_turns {
        let pct = (5 + (turn as u16 * 85 / max_turns as u16).min(85)) as u8;
        task.update_progress(pct, &format!("turn {turn}/{max_turns}"))
            .await?;

        let (response, usage) = llm.chat(&messages).await?;
        total_usage.prompt_tokens += usage.prompt_tokens;
        total_usage.completion_tokens += usage.completion_tokens;
        total_usage.total_tokens += usage.total_tokens;

        if let Some(bid) = budget_id {
            let dims: &[(&str, u64)] = &[
                ("prompt_tokens", usage.prompt_tokens),
                ("completion_tokens", usage.completion_tokens),
                ("total_tokens", usage.total_tokens),
            ];
            match task.report_usage(bid, dims, None).await {
                Ok(ff_core::contracts::ReportUsageResult::Ok) => {}
                Ok(result) => {
                    tracing::warn!(execution_id = %eid, ?result, "budget limit reached");
                }
                Err(e) => {
                    tracing::warn!(execution_id = %eid, error = %e, "report_usage failed");
                }
            }
        }

        let (thought, action, action_input) = parse_agent_response(&response);

        let observation = match action.as_str() {
            "edit" => "Code recorded. Use submit when ready.".to_string(),
            "search" => "Search results: [see repo_context in issue]".to_string(),
            "view" => "File contents: [see repo_context in issue]".to_string(),
            "submit" => {
                final_patch = Some(action_input.clone());
                "Solution submitted.".to_string()
            }
            other => format!("Unknown action '{other}'. Use edit, search, view, or submit."),
        };

        let step = AgentStep {
            turn,
            thought: thought.clone(),
            action: action.clone(),
            action_input: action_input.clone(),
            observation: observation.clone(),
        };
        steps_taken = turn;

        task.append_frame("agent_step", &serde_json::to_vec(&step)?, None)
            .await?;

        tracing::info!(
            execution_id = %eid,
            turn,
            action = %action,
            tokens = usage.total_tokens,
            "agent step"
        );

        if final_patch.is_some() {
            break;
        }

        messages.push(Message {
            role: "assistant".into(),
            content: response,
        });
        messages.push(Message {
            role: "user".into(),
            content: format!("Observation: {observation}"),
        });
    }

    if let Some(patch) = final_patch {
        let patch_result = PatchResult {
            patch,
            steps_taken,
            model_used: model_name.to_string(),
            total_tokens: total_usage.total_tokens,
        };

        // Stash for re-claim after review
        pending
            .lock()
            .await
            .insert(eid.clone(), patch_result);

        tracing::info!(execution_id = %eid, "patch ready — suspending for human review");

        match task
            .suspend(
                "awaiting_human_review",
                &[ConditionMatcher {
                    signal_name: "review_response".to_string(),
                }],
                Some(3_600_000), // 1 hour timeout
                TimeoutBehavior::Fail,
            )
            .await
        {
            Ok(SuspendOutcome::Suspended {
                waitpoint_id,
                waitpoint_token,
                ..
            }) => {
                println!(
                    "REVIEW NEEDED: execution_id={eid} waitpoint_id={waitpoint_id}"
                );
                // Use .as_str() — `WaitpointToken`'s Display impl is
                // redacted for log safety (RFC-004 §Waitpoint Security),
                // which would print `<redacted>` here. The approve CLI
                // needs the raw kid:hex so it can re-submit the HMAC.
                // Matches the media-pipeline convention.
                println!("WAITPOINT_TOKEN={}", waitpoint_token.as_str());
                println!(
                    "  # approve:\n  cargo run --bin approve -- --execution-id {eid} --waitpoint-id {waitpoint_id} --waitpoint-token {} --approve",
                    waitpoint_token.as_str()
                );
                println!(
                    "  # reject (same signal, approved=false inside payload):\n  cargo run --bin approve -- --execution-id {eid} --waitpoint-id {waitpoint_id} --waitpoint-token {} --reject --feedback \"...\"",
                    waitpoint_token.as_str()
                );
            }
            Ok(SuspendOutcome::AlreadySatisfied { .. }) => {
                tracing::info!(execution_id = %eid, "review already satisfied");
            }
            Err(e) => {
                pending.lock().await.remove(&eid);
                tracing::error!(execution_id = %eid, error = %e, "suspend failed");
            }
        }
    } else {
        tracing::warn!(execution_id = %eid, max_turns, "max turns exhausted");
        task.fail(
            &format!("Agent did not produce a solution within {max_turns} turns"),
            "max_turns_exhausted",
        )
        .await?;
    }

    Ok(())
}

fn parse_agent_response(response: &str) -> (String, String, String) {
    let mut thought = String::new();
    let mut action = String::new();
    let mut action_input = String::new();
    let mut in_action_input = false;

    for line in response.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("THOUGHT:") {
            thought = rest.trim().to_string();
            in_action_input = false;
        } else if let Some(rest) = trimmed.strip_prefix("ACTION_INPUT:") {
            action_input = rest.trim().to_string();
            in_action_input = true;
        } else if let Some(rest) = trimmed.strip_prefix("ACTION:") {
            action = rest.trim().to_lowercase();
            in_action_input = false;
        } else if in_action_input {
            action_input.push('\n');
            action_input.push_str(line);
        }
    }

    // Fallback: if the LLM didn't follow the format, treat the whole response as a submit
    if action.is_empty() {
        action = "submit".to_string();
        if action_input.is_empty() {
            action_input = response.to_string();
        }
    }

    (thought, action, action_input)
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => tracing::info!("received SIGINT"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.expect("failed to listen for Ctrl+C");
        tracing::info!("received Ctrl+C");
    }
}
