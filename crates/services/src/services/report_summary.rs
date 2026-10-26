//! Generates short summaries of workspaces' work for the activity report.
//!
//! A coding agent is run once per batch of workspaces in an empty scratch
//! directory outside any workspace, so generation does not show up as workspace
//! activity. Each workspace's prompts and the agent's own turn summaries are
//! included in the prompt, and the agent answers with a JSON object of
//! summaries in its final message. Every run is forced into the executor's
//! supervised (approval-required) mode with all approvals denied, so no tool
//! can act regardless of the selected profile's own permission settings;
//! executors without a supervised mode are refused.

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use db::models::activity_report::ActivityTurn;
use executors::{
    approvals::{ExecutorApprovalError, ExecutorApprovalService},
    env::{ExecutionEnv, RepoContext},
    executors::{
        BaseCodingAgent, CancellationToken, CodingAgent, ExecutorExitResult,
        StandardCodingAgentExecutor, codex::SandboxMode,
    },
    logs::{NormalizedEntryType, utils::patch::extract_normalized_entry_from_patch},
    model_selector::PermissionPolicy,
    profile::{ExecutorConfig, ExecutorConfigs, ExecutorProfileId},
};
use futures::{TryStreamExt, stream::select};
use thiserror::Error;
use tokio_util::io::ReaderStream;
use utils::{
    approvals::{ApprovalStatus, QuestionStatus},
    log_msg::LogMsg,
    msg_store::MsgStore,
};
use uuid::Uuid;

const MAX_PROMPT_CHARS: usize = 1500;
const MAX_TURN_SUMMARY_CHARS: usize = 1000;
/// Per-workspace input budget; middle turns are dropped beyond it.
const MAX_WORKSPACE_CHARS: usize = 12_000;
const GENERATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Error)]
pub enum ReportSummaryError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to start agent: {0}")]
    Spawn(String),
    #[error("Agent did not finish within {0} minutes")]
    Timeout(u64),
    #[error("Agent did not reply with usable summaries: {0}")]
    InvalidOutput(String),
}

/// How long to let output draining and log normalization catch up after the
/// agent exits.
const NORMALIZE_GRACE: Duration = Duration::from_secs(10);

/// Executors whose supervised mode routes every state-changing tool through
/// the approval service (or, for Droid, runs read-only).
pub fn supports_tool_free_runs(profile: &ExecutorProfileId) -> bool {
    matches!(
        profile.executor,
        BaseCodingAgent::ClaudeCode
            | BaseCodingAgent::Codex
            | BaseCodingAgent::Gemini
            | BaseCodingAgent::QwenCode
            | BaseCodingAgent::KimiCode
            | BaseCodingAgent::Grok
            | BaseCodingAgent::Opencode
            | BaseCodingAgent::Copilot
            | BaseCodingAgent::Droid
    )
}

/// Overrides the profile's permission settings so the agent must ask before
/// using tools; combined with `DenyAllApprovals` every such request fails.
fn restrict_tools(agent: &mut CodingAgent, profile: &ExecutorProfileId) {
    agent.apply_overrides(&ExecutorConfig {
        permission_policy: Some(PermissionPolicy::Supervised),
        ..ExecutorConfig::from(profile.clone())
    });
    match agent {
        CodingAgent::ClaudeCode(claude) => claude.dangerously_skip_permissions = Some(false),
        CodingAgent::Codex(codex) => codex.sandbox = Some(SandboxMode::ReadOnly),
        _ => {}
    }
    agent.use_approvals(Arc::new(DenyAllApprovals));
}

/// Denies every tool and question request. Summaries are written from the
/// prompt alone, so no tool use is ever needed.
struct DenyAllApprovals;

#[async_trait]
impl ExecutorApprovalService for DenyAllApprovals {
    async fn create_tool_approval(
        &self,
        _tool_name: &str,
    ) -> Result<String, ExecutorApprovalError> {
        Ok("report-summary".to_string())
    }

    async fn create_question_approval(
        &self,
        _tool_name: &str,
        _question_count: usize,
    ) -> Result<String, ExecutorApprovalError> {
        Ok("report-summary".to_string())
    }

    async fn wait_tool_approval(
        &self,
        _approval_id: &str,
        _cancel: CancellationToken,
    ) -> Result<ApprovalStatus, ExecutorApprovalError> {
        Ok(ApprovalStatus::Denied {
            reason: Some(
                "Tools are disabled while writing report summaries. Answer from the prompt only."
                    .to_string(),
            ),
        })
    }

    async fn wait_question_answer(
        &self,
        _approval_id: &str,
        _cancel: CancellationToken,
    ) -> Result<QuestionStatus, ExecutorApprovalError> {
        Err(ExecutorApprovalError::ServiceUnavailable)
    }
}

pub struct WorkspaceSummaryInput {
    pub workspace_id: Uuid,
    pub title: String,
    /// The task's original request, when it predates the period. Background
    /// only: the summary must describe just `turns`.
    pub goal: Option<String>,
    /// Turns that overlap the report period, oldest first.
    pub turns: Vec<ActivityTurn>,
}

/// A generated summary of one workspace's work in a period.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedSummary {
    /// A few words naming the work, e.g. "Fixing pnpm tests".
    pub title: String,
    /// One to three sentences describing what was done.
    pub summary: String,
}

const MAX_GOAL_CHARS: usize = 600;

/// Builds the summary input for a period from a workspace's full history:
/// turns whose agent run overlaps `[from, to)`, plus the original request as
/// background when the task started earlier. `None` if nothing overlaps.
pub fn input_for_period(
    workspace_id: Uuid,
    title: String,
    all_turns: Vec<ActivityTurn>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<WorkspaceSummaryInput> {
    let goal = all_turns
        .first()
        .filter(|t| t.created_at < from)
        .and_then(|t| t.prompt.clone());
    let turns: Vec<ActivityTurn> = all_turns
        .into_iter()
        .filter(|t| t.created_at < to && t.completed_at.unwrap_or(t.created_at) >= from)
        .collect();
    (!turns.is_empty()).then_some(WorkspaceSummaryInput {
        workspace_id,
        title,
        goal,
        turns,
    })
}

fn truncate(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}… [truncated]")
}

fn render_turn(index: usize, turn: &ActivityTurn) -> String {
    let mut out = format!("#### Turn {}\n", index + 1);
    if let Some(prompt) = turn.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        out.push_str(&format!(
            "User prompt:\n{}\n\n",
            truncate(prompt, MAX_PROMPT_CHARS)
        ));
    }
    if let Some(summary) = turn.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!(
            "Agent result:\n{}\n\n",
            truncate(summary, MAX_TURN_SUMMARY_CHARS)
        ));
    }
    out
}

/// Renders a workspace's turns, keeping the first and latest turns when the
/// full history exceeds the budget.
fn render_workspace(input: &WorkspaceSummaryInput) -> String {
    let rendered: Vec<String> = input
        .turns
        .iter()
        .enumerate()
        .map(|(i, t)| render_turn(i, t))
        .collect();

    let mut kept_head = Vec::new();
    let mut kept_tail = Vec::new();
    let mut used = 0;
    let (mut lo, mut hi) = (0, rendered.len());
    // Alternate between the start and the end so both the goal and the outcome survive.
    while lo < hi {
        let take_head = kept_head.len() <= kept_tail.len();
        let candidate = if take_head {
            &rendered[lo]
        } else {
            &rendered[hi - 1]
        };
        let candidate_chars = candidate.chars().count();
        if used + candidate_chars > MAX_WORKSPACE_CHARS {
            break;
        }
        used += candidate_chars;
        if take_head {
            kept_head.push(candidate.clone());
            lo += 1;
        } else {
            kept_tail.push(candidate.clone());
            hi -= 1;
        }
    }
    let omitted = hi - lo;

    let mut out = format!(
        "## Workspace `{}`\nWorkspace name: {}\n\n",
        input.workspace_id, input.title
    );
    if let Some(goal) = input.goal.as_deref().filter(|g| !g.trim().is_empty()) {
        out.push_str(&format!(
            "Original request (background only; it was made before this period):\n{}\n\n",
            truncate(goal, MAX_GOAL_CHARS)
        ));
    }
    out.push_str("### Work in this period\n\n");
    out.push_str(&kept_head.concat());
    if omitted > 0 {
        out.push_str(&format!("_({omitted} turns omitted)_\n\n"));
    }
    kept_tail.reverse();
    out.push_str(&kept_tail.concat());
    out
}

pub fn build_input_document(inputs: &[WorkspaceSummaryInput]) -> String {
    let mut doc = String::from(
        "# Workspace activity\n\nEach section below is one task (workspace) a developer worked on \
         with a coding agent during the report period: the developer's prompts and the agent's \
         results, oldest first.\n\n",
    );
    for input in inputs {
        doc.push_str(&render_workspace(input));
        doc.push('\n');
    }
    doc
}

pub fn build_prompt(inputs: &[WorkspaceSummaryInput]) -> String {
    let ids = inputs
        .iter()
        .map(|i| format!("\"{}\"", i.workspace_id))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You are writing entries for a developer's work report for one period. Below is the \
         activity of {count} tasks the developer worked on with a coding agent in that period.\n\n\
         Describe only the work under \"Work in this period\". An \"Original request\" is \
         background for understanding; do not describe work it asks for unless that work \
         appears in the period.\n\n\
         For each task write:\n\
         - \"title\": 2-5 words naming the work done in the period, like a timesheet line \
         (e.g. \"Fixing pnpm tests\", \"Cursor lag investigation\"). Not the workspace name.\n\
         - \"summary\": 1-3 sentences describing what was accomplished in the period (the \
         outcome, not the conversation), in past tense. Do not mention the agent, prompts or \
         turns.\n\n\
         Do not use any tools, read or write files, or run commands: everything you need is \
         below. Reply with only a JSON object mapping each workspace id to \
         {{\"title\": ..., \"summary\": ...}}, with exactly these keys: [{ids}].\n\n---\n\n{document}",
        count = inputs.len(),
        document = build_input_document(inputs),
    )
}

/// Parses the agent's output, tolerating a markdown code fence around the JSON.
pub fn parse_output(raw: &str) -> Result<HashMap<Uuid, GeneratedSummary>, ReportSummaryError> {
    let trimmed = raw.trim();
    let json = match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(start), Some(end)) if end > start => &trimmed[start..=end],
        _ => {
            return Err(ReportSummaryError::InvalidOutput(
                "no JSON object found".to_string(),
            ));
        }
    };
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(default)]
        title: String,
        #[serde(default)]
        summary: String,
    }
    let map: HashMap<String, Entry> =
        serde_json::from_str(json).map_err(|e| ReportSummaryError::InvalidOutput(e.to_string()))?;
    Ok(map
        .into_iter()
        .filter_map(|(k, v)| {
            let id = Uuid::parse_str(k.trim()).ok()?;
            let summary = v.summary.trim().to_string();
            (!summary.is_empty()).then(|| {
                (
                    id,
                    GeneratedSummary {
                        title: v.title.trim().to_string(),
                        summary,
                    },
                )
            })
        })
        .collect())
}

/// Runs `profile` once to summarize `inputs`. Returns summaries keyed by
/// workspace id; workspaces the agent skipped are absent.
pub async fn generate_summaries(
    profile: &ExecutorProfileId,
    inputs: &[WorkspaceSummaryInput],
) -> Result<HashMap<Uuid, GeneratedSummary>, ReportSummaryError> {
    let dir = std::env::temp_dir().join(format!("vk-report-summary-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await?;
    let result = run_in_dir(&dir, profile, inputs).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    result
}

async fn run_in_dir(
    dir: &Path,
    profile: &ExecutorProfileId,
    inputs: &[WorkspaceSummaryInput],
) -> Result<HashMap<Uuid, GeneratedSummary>, ReportSummaryError> {
    if !supports_tool_free_runs(profile) {
        return Err(ReportSummaryError::Spawn(format!(
            "{} cannot be restricted to running without tools",
            profile.executor
        )));
    }
    let mut agent = ExecutorConfigs::get_cached()
        .get_coding_agent(profile)
        .ok_or_else(|| ReportSummaryError::Spawn(format!("unknown agent profile {profile}")))?;
    restrict_tools(&mut agent, profile);

    let env = ExecutionEnv::new(
        RepoContext::new(dir.to_path_buf(), vec![]),
        false,
        String::new(),
    );
    let mut spawned = agent
        .spawn(dir, &build_prompt(inputs), &env)
        .await
        .map_err(|e| ReportSummaryError::Spawn(e.to_string()))?;

    // Capture output and normalize it, as for regular executions, so the
    // agent's final message can be read regardless of the executor's format.
    let store = Arc::new(MsgStore::new());
    let mut forwarder = None;
    if let (Some(out), Some(err)) = (
        spawned.child.inner().stdout.take(),
        spawned.child.inner().stderr.take(),
    ) {
        let out = ReaderStream::new(out)
            .map_ok(|chunk| LogMsg::Stdout(String::from_utf8_lossy(&chunk).into_owned()));
        let err = ReaderStream::new(err)
            .map_ok(|chunk| LogMsg::Stderr(String::from_utf8_lossy(&chunk).into_owned()));
        forwarder = Some(store.clone().spawn_forwarder(select(out, err)));
    }
    let normalizers = agent.normalize_logs(store.clone(), dir);

    // Some agents exit on their own; others keep running and signal completion.
    let exit_signal = spawned.exit_signal.take();
    let finished = tokio::time::timeout(GENERATION_TIMEOUT, async {
        match exit_signal {
            Some(signal) => {
                tokio::select! {
                    result = signal => !matches!(result, Ok(ExecutorExitResult::Failure)),
                    status = spawned.child.wait() => status.map(|s| s.success()).unwrap_or(false),
                }
            }
            None => spawned
                .child
                .wait()
                .await
                .map(|s| s.success())
                .unwrap_or(false),
        }
    })
    .await;

    if let Some(cancel) = spawned.cancel.as_ref() {
        cancel.cancel();
    }
    let _ = utils::process::kill_process_group(&mut spawned.child).await;
    // The pipes close once the process group is gone; wait for the forwarder to
    // drain them so the final reply lands before the Finished marker.
    if let Some(forwarder) = forwarder {
        let _ = tokio::time::timeout(NORMALIZE_GRACE, forwarder).await;
    }
    store.push_finished();
    let _ = tokio::time::timeout(NORMALIZE_GRACE, futures::future::join_all(normalizers)).await;

    if finished.is_err() {
        return Err(ReportSummaryError::Timeout(
            GENERATION_TIMEOUT.as_secs() / 60,
        ));
    }

    let reply = last_assistant_message(&store.get_history()).ok_or_else(|| {
        ReportSummaryError::InvalidOutput("the agent produced no reply".to_string())
    })?;
    parse_output(&reply)
}

/// The agent's final non-empty assistant message.
fn last_assistant_message(history: &[LogMsg]) -> Option<String> {
    history.iter().rev().find_map(|msg| {
        let LogMsg::JsonPatch(patch) = msg else {
            return None;
        };
        let (_, entry) = extract_normalized_entry_from_patch(patch)?;
        let content = entry.content.trim();
        (matches!(entry.entry_type, NormalizedEntryType::AssistantMessage) && !content.is_empty())
            .then(|| content.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(prompt: &str, summary: &str) -> ActivityTurn {
        ActivityTurn {
            workspace_id: Uuid::nil(),
            created_at: Utc::now(),
            completed_at: Some(Utc::now()),
            prompt: Some(prompt.to_string()),
            summary: Some(summary.to_string()),
        }
    }

    #[test]
    fn parses_fenced_json_and_skips_bad_keys() {
        let id = Uuid::new_v4();
        let raw = format!(
            "```json\n{{\"{id}\": {{\"title\": \" Login fix \", \"summary\": \" Fixed the login bug. \"}}, \
             \"nope\": {{\"title\": \"x\", \"summary\": \"x\"}}}}\n```"
        );
        let parsed = parse_output(&raw).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[&id],
            GeneratedSummary {
                title: "Login fix".to_string(),
                summary: "Fixed the login bug.".to_string(),
            }
        );
    }

    #[test]
    fn rejects_output_without_json() {
        assert!(parse_output("I could not do it").is_err());
    }

    #[test]
    fn keeps_first_and_last_turns_when_over_budget() {
        let long = "x".repeat(MAX_PROMPT_CHARS);
        let mut turns: Vec<ActivityTurn> = (0..30).map(|_| turn(&long, "done")).collect();
        turns[0] = turn("FIRST GOAL", "started");
        turns[29] = turn("LAST STEP", "finished");
        let doc = render_workspace(&WorkspaceSummaryInput {
            workspace_id: Uuid::nil(),
            title: "Task".to_string(),
            goal: None,
            turns,
        });
        assert!(doc.contains("FIRST GOAL"));
        assert!(doc.contains("LAST STEP"));
        assert!(doc.contains("turns omitted"));
    }

    #[test]
    fn budgets_by_characters_not_bytes() {
        // Each turn is ~1,500 CJK characters (~4,500 bytes) after truncation;
        // four turns fit the character budget but not a byte budget.
        let cjk = "\u{4e2d}".repeat(MAX_PROMPT_CHARS);
        let mut turns: Vec<ActivityTurn> = (0..4).map(|_| turn(&cjk, "ok")).collect();
        turns[3] = turn(&cjk, "finished");
        let doc = render_workspace(&WorkspaceSummaryInput {
            workspace_id: Uuid::nil(),
            title: "Task".to_string(),
            goal: None,
            turns,
        });
        assert!(!doc.contains("turns omitted"));
        assert!(doc.contains("finished"));
    }

    fn restricted(executor: BaseCodingAgent) -> CodingAgent {
        let profile = ExecutorProfileId::new(executor);
        let mut agent = ExecutorConfigs::get_cached()
            .get_coding_agent(&profile)
            .expect("default profile");
        restrict_tools(&mut agent, &profile);
        agent
    }

    #[test]
    fn default_profiles_are_forced_to_ask_for_tools() {
        let CodingAgent::ClaudeCode(claude) = restricted(BaseCodingAgent::ClaudeCode) else {
            panic!("expected Claude");
        };
        assert_eq!(claude.dangerously_skip_permissions, Some(false));
        assert_eq!(claude.approvals, Some(true));

        let CodingAgent::Gemini(gemini) = restricted(BaseCodingAgent::Gemini) else {
            panic!("expected Gemini");
        };
        assert_eq!(gemini.yolo, Some(false));

        let CodingAgent::Codex(codex) = restricted(BaseCodingAgent::Codex) else {
            panic!("expected Codex");
        };
        assert_eq!(codex.sandbox, Some(SandboxMode::ReadOnly));
    }

    #[test]
    fn refuses_executors_without_supervised_mode() {
        assert!(!supports_tool_free_runs(&ExecutorProfileId::new(
            BaseCodingAgent::Amp
        )));
        assert!(!supports_tool_free_runs(&ExecutorProfileId::new(
            BaseCodingAgent::CursorAgent
        )));
        assert!(supports_tool_free_runs(&ExecutorProfileId::new(
            BaseCodingAgent::ClaudeCode
        )));
    }

    #[test]
    fn period_input_keeps_only_overlapping_turns_and_old_goal() {
        use chrono::TimeZone;
        let t = |d, h| Utc.with_ymd_and_hms(2026, 10, d, h, 0, 0).unwrap();
        let turn_at = |created, completed, prompt: &str| ActivityTurn {
            workspace_id: Uuid::nil(),
            created_at: created,
            completed_at: completed,
            prompt: Some(prompt.to_string()),
            summary: Some("done".to_string()),
        };
        let turns = vec![
            turn_at(t(21, 9), Some(t(21, 10)), "Fix cursor lag"),
            turn_at(t(22, 23), Some(t(23, 1)), "Run the tests"),
            turn_at(t(23, 9), Some(t(23, 10)), "Fix pnpm tests"),
            turn_at(t(24, 9), Some(t(24, 10)), "Tomorrow"),
        ];
        let input =
            input_for_period(Uuid::nil(), "ws".to_string(), turns, t(23, 0), t(24, 0)).unwrap();
        let prompts: Vec<_> = input
            .turns
            .iter()
            .filter_map(|t| t.prompt.clone())
            .collect();
        assert_eq!(prompts, ["Run the tests", "Fix pnpm tests"]);
        assert_eq!(input.goal.as_deref(), Some("Fix cursor lag"));

        let doc = render_workspace(&input);
        assert!(doc.contains("background only"));
        assert!(doc.contains("Work in this period"));
    }

    #[test]
    fn period_without_turns_yields_no_input() {
        use chrono::TimeZone;
        let created = Utc.with_ymd_and_hms(2026, 10, 21, 9, 0, 0).unwrap();
        let turns = vec![ActivityTurn {
            workspace_id: Uuid::nil(),
            created_at: created,
            completed_at: Some(created),
            prompt: Some("old".to_string()),
            summary: None,
        }];
        let from = Utc.with_ymd_and_hms(2026, 10, 23, 0, 0, 0).unwrap();
        assert!(
            input_for_period(
                Uuid::nil(),
                "ws".into(),
                turns,
                from,
                from + chrono::Duration::days(1)
            )
            .is_none()
        );
    }
}
