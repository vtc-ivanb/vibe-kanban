use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use axum::{
    Json, Router,
    extract::{Query, State},
    response::Json as ResponseJson,
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use db::models::activity_report::{self, WorkspaceReportSummary};
use deployment::Deployment;
use executors::profile::{ExecutorConfigs, ExecutorProfileId};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use services::services::{
    activity_report::{self as report, ReportInput},
    report_summary,
};
use sqlx::SqlitePool;
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError};

const DEFAULT_IDLE_CAP_MINUTES: u32 = 15;
const MAX_IDLE_CAP_MINUTES: u32 = 240;
const MAX_REPORT_DAYS: i64 = 400;
const SUMMARY_BATCH_SIZE: usize = 6;
const SUMMARY_CONCURRENCY: usize = 2;

pub fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route("/reports/activity", get(get_activity_report))
        .route("/reports/summaries", post(generate_summaries))
        .route("/reports/summaries/status", get(get_summary_status))
}

#[derive(Debug, Deserialize, TS)]
pub struct ActivityReportQuery {
    /// Start of the period (inclusive), e.g. local midnight of the first day.
    pub from: DateTime<Utc>,
    /// End of the period (exclusive), e.g. local midnight after the last day.
    pub to: DateTime<Utc>,
    /// Gaps longer than this between agent output and the user's next action
    /// are treated as the user being away.
    #[serde(default)]
    pub idle_cap_minutes: Option<u32>,
}

#[derive(Debug, Serialize, TS)]
pub struct WorkspaceActivitySummary {
    /// A few words naming the work done in the period, e.g. "Fixing pnpm tests".
    pub title: String,
    /// One to three sentences describing the work done in the period.
    pub text: String,
    pub generated_at: DateTime<Utc>,
    /// The period has had agent activity since the summary was generated.
    pub is_stale: bool,
}

#[derive(Debug, Serialize, TS)]
pub struct WorkspaceActivity {
    pub workspace_id: Uuid,
    /// Workspace name, or its branch when unnamed.
    pub name: String,
    pub branch: String,
    pub archived: bool,
    pub repos: Vec<String>,
    pub first_activity_at: Option<DateTime<Utc>>,
    pub last_activity_at: Option<DateTime<Utc>>,
    /// Estimated time the user spent interacting with this workspace.
    pub user_seconds: u32,
    /// Time coding agents spent running in this workspace.
    pub agent_seconds: u32,
    pub prompt_count: u32,
    pub merged_at: Option<DateTime<Utc>>,
    pub summary: Option<WorkspaceActivitySummary>,
}

#[derive(Debug, Serialize, TS)]
pub struct ActivityReport {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub idle_cap_minutes: u32,
    /// Wall-clock time with any user activity (parallel work counted once).
    pub user_seconds: u32,
    pub agent_seconds: u32,
    /// Sorted by estimated user time, most first.
    pub workspaces: Vec<WorkspaceActivity>,
}

pub async fn get_activity_report(
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<ActivityReportQuery>,
) -> Result<ResponseJson<ApiResponse<ActivityReport>>, ApiError> {
    let (from, to) = (query.from, query.to);
    if to <= from {
        return Err(ApiError::BadRequest(
            "`to` must be after `from`".to_string(),
        ));
    }
    if to - from > Duration::days(MAX_REPORT_DAYS + 1) {
        return Err(ApiError::BadRequest(format!(
            "Reports are limited to {MAX_REPORT_DAYS} days"
        )));
    }
    let idle_cap_minutes = query
        .idle_cap_minutes
        .unwrap_or(DEFAULT_IDLE_CAP_MINUTES)
        .clamp(1, MAX_IDLE_CAP_MINUTES);
    let pool = &deployment.db().pool;

    // Load from earlier than `from` so the first actions in the period can see
    // the agent output that preceded them.
    let load_from = from - Duration::minutes(MAX_IDLE_CAP_MINUTES as i64) - Duration::days(1);
    let processes = activity_report::find_processes_between(pool, load_from, to).await?;
    let merges = activity_report::find_merges_between(pool, load_from, to).await?;

    let computed = report::compute(&ReportInput {
        from,
        to,
        idle_cap: Duration::minutes(idle_cap_minutes as i64),
        now: Utc::now(),
        processes: &processes,
        merges: &merges,
    });

    let ids: Vec<Uuid> = computed.workspaces.keys().copied().collect();
    let workspaces = activity_report::find_workspaces(pool, &ids).await?;
    let mut repos_by_ws: HashMap<Uuid, Vec<String>> = HashMap::new();
    for row in activity_report::find_workspace_repos(pool, &ids).await? {
        repos_by_ws
            .entry(row.workspace_id)
            .or_default()
            .push(row.repo_name);
    }
    let latest_merge: HashMap<Uuid, DateTime<Utc>> =
        activity_report::find_latest_merges(pool, &ids)
            .await?
            .into_iter()
            .map(|m| (m.workspace_id, m.at))
            .collect();
    let summaries: HashMap<Uuid, WorkspaceReportSummary> =
        WorkspaceReportSummary::find_for_period(pool, &ids, from, to)
            .await?
            .into_iter()
            .map(|s| (s.workspace_id, s))
            .collect();
    // Newer agent activity in the period, including a run that was in flight
    // when the summary was generated, marks the summary stale.
    let latest_activity = report::period_activity_versions(&processes, from, to);

    let mut rows: Vec<WorkspaceActivity> = workspaces
        .into_iter()
        .filter_map(|ws| {
            let totals = computed.workspaces.get(&ws.id)?;
            let summary = summaries.get(&ws.id).map(|s| WorkspaceActivitySummary {
                title: s.title.clone(),
                text: s.summary.clone(),
                generated_at: s.updated_at,
                is_stale: latest_activity
                    .get(&ws.id)
                    .is_some_and(|&at| at > s.source_activity_at),
            });
            Some(WorkspaceActivity {
                workspace_id: ws.id,
                name: ws
                    .name
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| ws.branch.clone()),
                branch: ws.branch,
                archived: ws.archived,
                repos: repos_by_ws.remove(&ws.id).unwrap_or_default(),
                first_activity_at: totals.first_activity_at,
                last_activity_at: totals.last_activity_at,
                user_seconds: totals.user_seconds.round() as u32,
                agent_seconds: totals.agent_seconds.round() as u32,
                prompt_count: totals.prompt_count,
                merged_at: latest_merge.get(&ws.id).copied(),
                summary,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.user_seconds
            .cmp(&a.user_seconds)
            .then(b.agent_seconds.cmp(&a.agent_seconds))
    });

    Ok(ResponseJson(ApiResponse::success(ActivityReport {
        from,
        to,
        idle_cap_minutes,
        user_seconds: computed.user_seconds.round() as u32,
        agent_seconds: computed.agent_seconds.round() as u32,
        workspaces: rows,
    })))
}

#[derive(Debug, Deserialize, TS)]
pub struct GenerateReportSummariesRequest {
    pub workspace_ids: Vec<Uuid>,
    pub executor_profile_id: ExecutorProfileId,
    /// Report period to summarize (`[from, to)`); only work in it is described.
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, TS)]
pub struct ReportSummaryJobStatus {
    pub running: bool,
    pub total: u32,
    pub completed: u32,
    pub failed: u32,
    /// The most recent generation error, if any.
    pub last_error: Option<String>,
}

static SUMMARY_JOB: LazyLock<Mutex<ReportSummaryJobStatus>> =
    LazyLock::new(|| Mutex::new(ReportSummaryJobStatus::default()));

fn job_status() -> ReportSummaryJobStatus {
    SUMMARY_JOB.lock().expect("summary job lock").clone()
}

fn update_job(f: impl FnOnce(&mut ReportSummaryJobStatus)) {
    f(&mut SUMMARY_JOB.lock().expect("summary job lock"));
}

pub async fn get_summary_status() -> ResponseJson<ApiResponse<ReportSummaryJobStatus>> {
    ResponseJson(ApiResponse::success(job_status()))
}

pub async fn generate_summaries(
    State(deployment): State<DeploymentImpl>,
    Json(request): Json<GenerateReportSummariesRequest>,
) -> Result<ResponseJson<ApiResponse<ReportSummaryJobStatus>>, ApiError> {
    if !report_summary::supports_tool_free_runs(&request.executor_profile_id) {
        return Err(ApiError::BadRequest(format!(
            "{} can't be restricted to running without tools, so it can't generate \
             summaries. Pick another agent.",
            request.executor_profile_id.executor
        )));
    }
    if ExecutorConfigs::get_cached()
        .get_coding_agent(&request.executor_profile_id)
        .is_none()
    {
        return Err(ApiError::BadRequest(format!(
            "Unknown agent profile: {}",
            request.executor_profile_id
        )));
    }
    let (from, to) = (request.from, request.to);
    if to <= from {
        return Err(ApiError::BadRequest(
            "`to` must be after `from`".to_string(),
        ));
    }
    let mut ids = request.workspace_ids;
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Ok(ResponseJson(ApiResponse::success(job_status())));
    }

    {
        let mut job = SUMMARY_JOB.lock().expect("summary job lock");
        if job.running {
            return Err(ApiError::Conflict(
                "Summaries are already being generated".to_string(),
            ));
        }
        *job = ReportSummaryJobStatus {
            running: true,
            total: ids.len() as u32,
            ..Default::default()
        };
    }

    let pool = deployment.db().pool.clone();
    let profile = request.executor_profile_id;
    tokio::spawn(async move {
        run_summary_job(pool, profile, ids, from, to).await;
        update_job(|job| job.running = false);
    });

    Ok(ResponseJson(ApiResponse::success(job_status())))
}

async fn run_summary_job(
    pool: SqlitePool,
    profile: ExecutorProfileId,
    ids: Vec<Uuid>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) {
    let profile_json = serde_json::to_string(&profile).unwrap_or_default();
    let batches: Vec<Vec<Uuid>> = ids
        .chunks(SUMMARY_BATCH_SIZE)
        .map(<[Uuid]>::to_vec)
        .collect();

    stream::iter(batches)
        .for_each_concurrent(SUMMARY_CONCURRENCY, |batch| {
            let pool = pool.clone();
            let profile = profile.clone();
            let profile_json = profile_json.clone();
            async move {
                let batch_len = batch.len() as u32;
                match summarize_batch(&pool, &profile, &profile_json, &batch, from, to).await {
                    Ok(written) => update_job(|job| {
                        job.completed += written;
                        job.failed += batch_len - written;
                    }),
                    Err(e) => {
                        tracing::warn!("Report summary generation failed: {e}");
                        update_job(|job| {
                            job.failed += batch_len;
                            job.last_error = Some(e);
                        });
                    }
                }
            }
        })
        .await;
}

/// Summarizes the work one batch of workspaces did in `[from, to)` and stores
/// the results. Returns how many summaries were written.
async fn summarize_batch(
    pool: &SqlitePool,
    profile: &ExecutorProfileId,
    profile_json: &str,
    batch: &[Uuid],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<u32, String> {
    let workspaces = activity_report::find_workspaces(pool, batch)
        .await
        .map_err(|e| e.to_string())?;

    // Captured before reading turns, so anything that changes afterwards
    // (including a run still in flight) leaves the summary stale.
    let processes = activity_report::find_processes_between(pool, from - Duration::days(1), to)
        .await
        .map_err(|e| e.to_string())?;
    let source_activity = report::period_activity_versions(&processes, from, to);

    let mut inputs = Vec::new();
    for ws in workspaces {
        if !source_activity.contains_key(&ws.id) {
            continue;
        }
        let turns = activity_report::find_turns_for_workspace(pool, ws.id)
            .await
            .map_err(|e| e.to_string())?;
        let name = ws
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(ws.branch);
        if let Some(input) = report_summary::input_for_period(ws.id, name, turns, from, to) {
            inputs.push(input);
        }
    }
    if inputs.is_empty() {
        return Ok(0);
    }

    let summaries = report_summary::generate_summaries(profile, &inputs)
        .await
        .map_err(|e| e.to_string())?;

    let mut written = 0;
    for (workspace_id, generated) in summaries {
        let Some(&source_activity_at) = source_activity.get(&workspace_id) else {
            continue;
        };
        WorkspaceReportSummary::upsert(
            pool,
            workspace_id,
            from,
            to,
            &generated.title,
            &generated.summary,
            source_activity_at,
            profile_json,
        )
        .await
        .map_err(|e| e.to_string())?;
        written += 1;
    }
    Ok(written)
}
