//! Read-only queries backing the activity report.
//!
//! These use runtime-checked queries (no `query!` macros) because they only read
//! existing tables and build `IN (...)` lists dynamically.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, QueryBuilder, Sqlite, SqlitePool};
use ts_rs::TS;
use uuid::Uuid;

use super::execution_process::{ExecutionProcessRunReason, ExecutionProcessStatus};

/// An execution process that is relevant to the activity report: coding agent
/// runs, merge-commit-message runs (started by a merge click) and dev servers.
#[derive(Debug, Clone, FromRow)]
pub struct ActivityProcess {
    pub workspace_id: Uuid,
    pub run_reason: ExecutionProcessRunReason,
    pub status: ExecutionProcessStatus,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Length of the prompt sent with this run, for coding agent runs.
    pub prompt_chars: Option<i64>,
}

/// A per-workspace point in time, e.g. a merge.
#[derive(Debug, Clone, FromRow)]
pub struct WorkspaceTimestamp {
    pub workspace_id: Uuid,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct ActivityWorkspace {
    pub id: Uuid,
    pub name: Option<String>,
    pub branch: String,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct ActivityWorkspaceRepo {
    pub workspace_id: Uuid,
    pub repo_name: String,
}

/// A prompt and the agent's own summary of the turn, used as input for
/// generating a workspace summary.
#[derive(Debug, Clone, FromRow)]
pub struct ActivityTurn {
    pub workspace_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub prompt: Option<String>,
    pub summary: Option<String>,
}

/// Timestamps are stored in two formats (`YYYY-MM-DD HH:MM:SS` from SQLite
/// defaults and RFC 3339 from the application). Both start with the date, so
/// bounds are compared at day granularity; callers filter precisely afterwards.
fn day_bound(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

pub async fn find_processes_between(
    pool: &SqlitePool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<ActivityProcess>, sqlx::Error> {
    sqlx::query_as::<_, ActivityProcess>(
        r#"SELECT s.workspace_id AS workspace_id,
                  ep.run_reason AS run_reason,
                  ep.status AS status,
                  ep.started_at AS started_at,
                  ep.completed_at AS completed_at,
                  (SELECT MAX(LENGTH(cat.prompt))
                     FROM coding_agent_turns cat
                    WHERE cat.execution_process_id = ep.id) AS prompt_chars
             FROM execution_processes ep
             JOIN sessions s ON s.id = ep.session_id
            WHERE ep.run_reason IN ('codingagent', 'mergecommitmessage', 'devserver')
              AND ep.started_at < ?
              AND (ep.started_at >= ?
                   OR ep.completed_at >= ?
                   OR ep.status = 'running')
            ORDER BY ep.started_at ASC"#,
    )
    .bind(day_bound(to + chrono::Duration::days(1)))
    .bind(day_bound(from))
    .bind(day_bound(from))
    .fetch_all(pool)
    .await
}

pub async fn find_merges_between(
    pool: &SqlitePool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<WorkspaceTimestamp>, sqlx::Error> {
    sqlx::query_as::<_, WorkspaceTimestamp>(
        r#"SELECT workspace_id, created_at AS at
             FROM merges
            WHERE created_at >= ?
              AND created_at < ?
            ORDER BY created_at ASC"#,
    )
    .bind(day_bound(from))
    .bind(day_bound(to + chrono::Duration::days(1)))
    .fetch_all(pool)
    .await
}

/// Latest direct merge or merged pull request per workspace, regardless of the
/// report period.
pub async fn find_latest_merges(
    pool: &SqlitePool,
    workspace_ids: &[Uuid],
) -> Result<Vec<WorkspaceTimestamp>, sqlx::Error> {
    if workspace_ids.is_empty() {
        return Ok(vec![]);
    }
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        "SELECT workspace_id, MAX(at) AS at
           FROM (SELECT workspace_id, created_at AS at FROM merges
                 UNION ALL
                 SELECT workspace_id, merged_at AS at
                   FROM pull_requests
                  WHERE pr_status = 'merged' AND merged_at IS NOT NULL)
          WHERE workspace_id IN (",
    );
    push_ids(&mut qb, workspace_ids);
    qb.push(") GROUP BY workspace_id");
    qb.build_query_as::<WorkspaceTimestamp>()
        .fetch_all(pool)
        .await
}

/// Latest coding agent activity per workspace: the most recent completion, or
/// the start of a run that is still going. It changes only when agent output
/// changes (not when turns are marked seen), so it versions cached summaries:
/// a run in flight at generation time moves it forward once it completes.
pub async fn find_latest_agent_activity(
    pool: &SqlitePool,
    workspace_ids: &[Uuid],
) -> Result<Vec<WorkspaceTimestamp>, sqlx::Error> {
    if workspace_ids.is_empty() {
        return Ok(vec![]);
    }
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        "SELECT s.workspace_id AS workspace_id,
                MAX(COALESCE(ep.completed_at, ep.started_at)) AS at
           FROM execution_processes ep
           JOIN sessions s ON s.id = ep.session_id
          WHERE ep.run_reason = 'codingagent'
            AND s.workspace_id IN (",
    );
    push_ids(&mut qb, workspace_ids);
    qb.push(") GROUP BY s.workspace_id");
    qb.build_query_as::<WorkspaceTimestamp>()
        .fetch_all(pool)
        .await
}

pub async fn find_workspaces(
    pool: &SqlitePool,
    workspace_ids: &[Uuid],
) -> Result<Vec<ActivityWorkspace>, sqlx::Error> {
    if workspace_ids.is_empty() {
        return Ok(vec![]);
    }
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        "SELECT id, name, branch, archived, created_at FROM workspaces WHERE id IN (",
    );
    push_ids(&mut qb, workspace_ids);
    qb.push(")");
    qb.build_query_as::<ActivityWorkspace>()
        .fetch_all(pool)
        .await
}

pub async fn find_workspace_repos(
    pool: &SqlitePool,
    workspace_ids: &[Uuid],
) -> Result<Vec<ActivityWorkspaceRepo>, sqlx::Error> {
    if workspace_ids.is_empty() {
        return Ok(vec![]);
    }
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        "SELECT wr.workspace_id AS workspace_id,
                COALESCE(r.display_name, r.name) AS repo_name
           FROM workspace_repos wr
           JOIN repos r ON r.id = wr.repo_id
          WHERE wr.workspace_id IN (",
    );
    push_ids(&mut qb, workspace_ids);
    qb.push(") ORDER BY wr.created_at ASC");
    qb.build_query_as::<ActivityWorkspaceRepo>()
        .fetch_all(pool)
        .await
}

/// All coding agent turns (prompt + agent summary) of a workspace, oldest first.
pub async fn find_turns_for_workspace(
    pool: &SqlitePool,
    workspace_id: Uuid,
) -> Result<Vec<ActivityTurn>, sqlx::Error> {
    sqlx::query_as::<_, ActivityTurn>(
        r#"SELECT s.workspace_id AS workspace_id,
                  cat.created_at AS created_at,
                  cat.prompt AS prompt,
                  cat.summary AS summary
             FROM coding_agent_turns cat
             JOIN execution_processes ep ON ep.id = cat.execution_process_id
             JOIN sessions s ON s.id = ep.session_id
            WHERE s.workspace_id = ?
              AND ep.run_reason = 'codingagent'
            ORDER BY cat.created_at ASC"#,
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await
}

fn push_ids(qb: &mut QueryBuilder<Sqlite>, ids: &[Uuid]) {
    let mut separated = qb.separated(", ");
    for id in ids {
        separated.push_bind(*id);
    }
}

/// A cached AI-generated summary of a workspace's work.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize, TS)]
pub struct WorkspaceReportSummary {
    pub workspace_id: Uuid,
    pub summary: String,
    /// Latest workspace activity the summary was generated from.
    pub source_activity_at: DateTime<Utc>,
    /// The executor profile (JSON) that generated the summary.
    pub executor_profile: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl WorkspaceReportSummary {
    pub async fn find_for_workspaces(
        pool: &SqlitePool,
        workspace_ids: &[Uuid],
    ) -> Result<Vec<Self>, sqlx::Error> {
        if workspace_ids.is_empty() {
            return Ok(vec![]);
        }
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT workspace_id, summary, source_activity_at, executor_profile, created_at, updated_at
               FROM workspace_report_summaries
              WHERE workspace_id IN (",
        );
        push_ids(&mut qb, workspace_ids);
        qb.push(")");
        qb.build_query_as::<Self>().fetch_all(pool).await
    }

    pub async fn upsert(
        pool: &SqlitePool,
        workspace_id: Uuid,
        summary: &str,
        source_activity_at: DateTime<Utc>,
        executor_profile: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"INSERT INTO workspace_report_summaries
                   (workspace_id, summary, source_activity_at, executor_profile)
               VALUES (?, ?, ?, ?)
               ON CONFLICT(workspace_id) DO UPDATE SET
                   summary = excluded.summary,
                   source_activity_at = excluded.source_activity_at,
                   executor_profile = excluded.executor_profile,
                   updated_at = datetime('now', 'subsec')"#,
        )
        .bind(workspace_id)
        .bind(summary)
        .bind(source_activity_at)
        .bind(executor_profile)
        .execute(pool)
        .await?;
        Ok(())
    }
}
