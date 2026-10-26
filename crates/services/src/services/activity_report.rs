//! Estimates how much time the user spent interacting with each workspace.
//!
//! Vibe Kanban does not record user presence, so time is inferred from actions
//! the user takes: sending a prompt (starting a coding agent run), merging, or
//! starting a dev server. Each action claims the time since the agent last
//! produced output in that workspace (the user was reading and replying),
//! capped at the idle cap — a longer gap means the user was away.
//!
//! Intervals are then merged across workspaces so parallel work is not counted
//! twice: time where several workspaces are active is split evenly between them.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use db::models::{
    activity_report::{ActivityProcess, WorkspaceTimestamp},
    execution_process::{ExecutionProcessRunReason, ExecutionProcessStatus},
};
use uuid::Uuid;

/// Time attributed to reading the agent's final output in a workspace when no
/// later action follows it.
const FINAL_REVIEW: Duration = Duration::minutes(2);
/// Time attributed to an action that has no preceding agent output, before
/// accounting for prompt length.
const BASE_ACTION_TIME: Duration = Duration::seconds(30);
/// Typing speed used to estimate time spent writing a first prompt.
const PROMPT_CHARS_PER_MINUTE: i64 = 200;
/// A merge within this window after another action is assumed to be the
/// result of that action (e.g. merge-commit-message generation), not a new one.
const MERGE_DEDUP_WINDOW: Duration = Duration::minutes(15);

#[derive(Debug, Clone, Default)]
pub struct WorkspaceTotals {
    pub user_seconds: f64,
    pub agent_seconds: f64,
    pub prompt_count: u32,
    pub first_activity_at: Option<DateTime<Utc>>,
    pub last_activity_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
pub struct ComputedReport {
    /// Wall-clock time with at least one active workspace.
    pub user_seconds: f64,
    pub agent_seconds: f64,
    pub workspaces: HashMap<Uuid, WorkspaceTotals>,
}

pub struct ReportInput<'a> {
    /// Start of the period (inclusive).
    pub from: DateTime<Utc>,
    /// End of the period (exclusive).
    pub to: DateTime<Utc>,
    pub idle_cap: Duration,
    pub now: DateTime<Utc>,
    pub processes: &'a [ActivityProcess],
    pub merges: &'a [WorkspaceTimestamp],
}

#[derive(Debug, Clone, Copy)]
struct Interval {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

pub fn compute(input: &ReportInput) -> ComputedReport {
    let (from, to) = (input.from, input.to);
    let mut processes_by_ws: HashMap<Uuid, Vec<&ActivityProcess>> = HashMap::new();
    for p in input.processes {
        processes_by_ws.entry(p.workspace_id).or_default().push(p);
    }
    let mut merges_by_ws: HashMap<Uuid, Vec<DateTime<Utc>>> = HashMap::new();
    for m in input.merges {
        merges_by_ws.entry(m.workspace_id).or_default().push(m.at);
    }

    let mut workspace_ids: Vec<Uuid> = processes_by_ws
        .keys()
        .chain(merges_by_ws.keys())
        .copied()
        .collect();
    workspace_ids.sort();
    workspace_ids.dedup();

    let mut report = ComputedReport::default();
    let mut user_intervals: Vec<(usize, Interval)> = Vec::new();

    for (idx, ws_id) in workspace_ids.iter().enumerate() {
        let processes = processes_by_ws.remove(ws_id).unwrap_or_default();
        let merges = merges_by_ws.remove(ws_id).unwrap_or_default();
        let totals = report.workspaces.entry(*ws_id).or_default();

        for interval in user_intervals_for_workspace(&processes, &merges, input) {
            if let Some(clipped) = clip(interval, from, to) {
                user_intervals.push((idx, clipped));
            }
        }

        for p in processes
            .iter()
            .filter(|p| p.run_reason == ExecutionProcessRunReason::CodingAgent)
        {
            if p.started_at >= from && p.started_at < to {
                totals.prompt_count += 1;
            }
            let run = Interval {
                start: p.started_at,
                end: process_end(p, input.now),
            };
            let Some(run) = clip(run, from, to) else {
                continue;
            };
            note_activity(totals, run);
            let secs = seconds(run.end - run.start);
            totals.agent_seconds += secs;
            report.agent_seconds += secs;
        }
    }

    attribute_user_time(&user_intervals, &workspace_ids, &mut report);

    report
        .workspaces
        .retain(|_, t| t.user_seconds > 0.0 || t.agent_seconds > 0.0);
    report
}

/// Latest coding agent activity within `[from, to)` per workspace: the most
/// recent completion of a run overlapping the period, or the start of a run
/// still going. It versions summaries of the period: it changes only when the
/// period gains agent output, including when a run in flight completes.
pub fn period_activity_versions(
    processes: &[ActivityProcess],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> HashMap<Uuid, DateTime<Utc>> {
    let mut versions: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
    for p in processes
        .iter()
        .filter(|p| p.run_reason == ExecutionProcessRunReason::CodingAgent)
    {
        let overlaps = p.started_at < to
            && match p.completed_at {
                Some(end) => end >= from,
                None => p.status == ExecutionProcessStatus::Running || p.started_at >= from,
            };
        if !overlaps {
            continue;
        }
        let at = p.completed_at.unwrap_or(p.started_at);
        versions
            .entry(p.workspace_id)
            .and_modify(|v| *v = (*v).max(at))
            .or_insert(at);
    }
    versions
}

fn process_end(p: &ActivityProcess, now: DateTime<Utc>) -> DateTime<Utc> {
    match (p.completed_at, &p.status) {
        (Some(end), _) => end.max(p.started_at),
        (None, ExecutionProcessStatus::Running) => now.max(p.started_at),
        (None, _) => p.started_at,
    }
}

struct UserAction {
    at: DateTime<Utc>,
    prompt_chars: Option<i64>,
}

fn user_intervals_for_workspace(
    processes: &[&ActivityProcess],
    merges: &[DateTime<Utc>],
    input: &ReportInput,
) -> Vec<Interval> {
    let mut actions: Vec<UserAction> = processes
        .iter()
        .map(|p| UserAction {
            at: p.started_at,
            prompt_chars: p.prompt_chars,
        })
        .collect();
    actions.sort_by_key(|a| a.at);

    for &merged_at in merges {
        let follows_action = actions
            .iter()
            .any(|a| a.at <= merged_at && merged_at - a.at <= MERGE_DEDUP_WINDOW);
        if !follows_action {
            actions.push(UserAction {
                at: merged_at,
                prompt_chars: None,
            });
        }
    }
    actions.sort_by_key(|a| a.at);

    // Moments the agent finished producing output the user then had to read.
    // Dev servers are excluded: they end when the user stops them.
    let mut outputs: Vec<DateTime<Utc>> = processes
        .iter()
        .filter(|p| p.run_reason != ExecutionProcessRunReason::DevServer)
        .filter_map(|p| p.completed_at)
        .collect();
    outputs.sort();

    let mut intervals = Vec::with_capacity(actions.len() + 1);
    let mut previous_action: Option<DateTime<Utc>> = None;
    for action in &actions {
        let last_output = outputs.iter().rev().find(|&&o| o <= action.at).copied();
        let anchor = match (last_output, previous_action) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let lookback = match anchor {
            Some(anchor) => (action.at - anchor).min(input.idle_cap),
            None => first_action_time(action.prompt_chars).min(input.idle_cap),
        };
        intervals.push(Interval {
            start: action.at - lookback,
            end: action.at,
        });
        previous_action = Some(action.at);
    }

    // Reading the final output, when nothing follows it.
    if let Some(&last_output) = outputs.last()
        && previous_action.is_none_or(|a| a < last_output)
    {
        intervals.push(Interval {
            start: last_output,
            end: last_output + FINAL_REVIEW.min(input.idle_cap),
        });
    }

    intervals
}

fn first_action_time(prompt_chars: Option<i64>) -> Duration {
    let typing = prompt_chars
        .map(|chars| Duration::seconds(chars.max(0) * 60 / PROMPT_CHARS_PER_MINUTE))
        .unwrap_or_default();
    BASE_ACTION_TIME + typing
}

fn clip(interval: Interval, from: DateTime<Utc>, to: DateTime<Utc>) -> Option<Interval> {
    let start = interval.start.max(from);
    let end = interval.end.min(to);
    (end > start).then_some(Interval { start, end })
}

fn note_activity(totals: &mut WorkspaceTotals, interval: Interval) {
    totals.first_activity_at = Some(
        totals
            .first_activity_at
            .map_or(interval.start, |t| t.min(interval.start)),
    );
    totals.last_activity_at = Some(
        totals
            .last_activity_at
            .map_or(interval.end, |t| t.max(interval.end)),
    );
}

fn seconds(d: Duration) -> f64 {
    d.num_milliseconds() as f64 / 1000.0
}

/// Sweeps over all user intervals, splitting each stretch of time evenly
/// between the workspaces active during it.
fn attribute_user_time(
    intervals: &[(usize, Interval)],
    workspace_ids: &[Uuid],
    report: &mut ComputedReport,
) {
    // (time, workspace index, +1 start / -1 end)
    let mut points: Vec<(DateTime<Utc>, usize, i32)> = Vec::with_capacity(intervals.len() * 2);
    for &(idx, interval) in intervals {
        points.push((interval.start, idx, 1));
        points.push((interval.end, idx, -1));
    }
    points.sort_by_key(|&(t, _, _)| t);

    let mut open_counts = vec![0i32; workspace_ids.len()];
    let mut active: Vec<usize> = Vec::new();
    let mut cursor: Option<DateTime<Utc>> = None;

    for (t, idx, delta) in points {
        if let Some(start) = cursor
            && t > start
            && !active.is_empty()
        {
            let secs = seconds(t - start);
            let share = secs / active.len() as f64;
            report.user_seconds += secs;
            for &ws_idx in &active {
                let totals = report.workspaces.entry(workspace_ids[ws_idx]).or_default();
                totals.user_seconds += share;
                note_activity(totals, Interval { start, end: t });
            }
        }
        cursor = Some(t);

        let count = &mut open_counts[idx];
        *count += delta;
        if delta > 0 && *count == 1 {
            active.push(idx);
        } else if delta < 0 && *count == 0 {
            active.retain(|&i| i != idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, h, m, 0).unwrap()
    }

    fn agent_run(
        ws: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        prompt_chars: Option<i64>,
    ) -> ActivityProcess {
        ActivityProcess {
            workspace_id: ws,
            run_reason: ExecutionProcessRunReason::CodingAgent,
            status: ExecutionProcessStatus::Completed,
            started_at: start,
            completed_at: Some(end),
            prompt_chars,
        }
    }

    fn run(processes: &[ActivityProcess], merges: &[WorkspaceTimestamp]) -> ComputedReport {
        compute(&ReportInput {
            from: at(0, 0),
            to: at(0, 0) + Duration::days(1),
            idle_cap: Duration::minutes(15),
            now: at(23, 59),
            processes,
            merges,
        })
    }

    #[test]
    fn follow_up_claims_gap_since_agent_output() {
        let ws = Uuid::new_v4();
        let report = run(
            &[
                agent_run(ws, at(10, 0), at(10, 10), Some(0)),
                agent_run(ws, at(10, 15), at(10, 20), None),
            ],
            &[],
        );
        let t = &report.workspaces[&ws];
        // 30s first prompt + 5 min reading/replying + 2 min final review.
        assert_eq!(t.user_seconds, 30.0 + 300.0 + 120.0);
        assert_eq!(t.agent_seconds, 900.0);
        assert_eq!(t.prompt_count, 2);
    }

    #[test]
    fn long_gaps_are_capped() {
        let ws = Uuid::new_v4();
        let report = run(
            &[
                agent_run(ws, at(10, 0), at(10, 10), Some(0)),
                agent_run(ws, at(12, 0), at(12, 5), None),
            ],
            &[],
        );
        assert_eq!(report.workspaces[&ws].user_seconds, 30.0 + 900.0 + 120.0);
    }

    #[test]
    fn first_prompt_time_scales_with_length() {
        let ws = Uuid::new_v4();
        let report = run(&[agent_run(ws, at(10, 0), at(10, 30), Some(400))], &[]);
        // 30s + 400 chars at 200 chars/min + final review.
        assert_eq!(report.workspaces[&ws].user_seconds, 30.0 + 120.0 + 120.0);
    }

    #[test]
    fn parallel_workspaces_split_overlapping_time() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let report = run(
            &[
                agent_run(a, at(9, 0), at(10, 0), Some(0)),
                agent_run(a, at(10, 10), at(11, 0), None),
                agent_run(b, at(9, 0), at(10, 0), Some(0)),
                agent_run(b, at(10, 10), at(11, 0), None),
            ],
            &[],
        );
        // Both claim 10:00-10:10 and 09:59:30-09:00, so wall clock counts once.
        let wall = report.user_seconds;
        let sum = report.workspaces[&a].user_seconds + report.workspaces[&b].user_seconds;
        assert!((wall - sum).abs() < 1e-6);
        assert_eq!(report.workspaces[&a].user_seconds, wall / 2.0);
    }

    #[test]
    fn merge_after_generation_is_not_double_counted() {
        let ws = Uuid::new_v4();
        let processes = [
            agent_run(ws, at(10, 0), at(10, 10), Some(0)),
            ActivityProcess {
                workspace_id: ws,
                run_reason: ExecutionProcessRunReason::MergeCommitMessage,
                status: ExecutionProcessStatus::Completed,
                started_at: at(10, 13),
                completed_at: Some(at(10, 14)),
                prompt_chars: None,
            },
        ];
        let merges = [WorkspaceTimestamp {
            workspace_id: ws,
            at: at(10, 14),
        }];
        let report = run(&processes, &merges);
        // First prompt + 3 min before merging + review of the generated message.
        assert_eq!(report.workspaces[&ws].user_seconds, 30.0 + 180.0 + 120.0);
    }

    #[test]
    fn activity_outside_period_is_excluded() {
        let ws = Uuid::new_v4();
        let processes = [agent_run(
            ws,
            Utc.with_ymd_and_hms(2026, 10, 19, 10, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 10, 19, 10, 5, 0).unwrap(),
            None,
        )];
        let report = run(&processes, &[]);
        assert!(report.workspaces.is_empty());
    }

    #[test]
    fn includes_runs_started_before_the_period() {
        let ws = Uuid::new_v4();
        let processes = [agent_run(
            ws,
            Utc.with_ymd_and_hms(2026, 10, 19, 23, 0, 0).unwrap(),
            at(1, 0),
            None,
        )];
        let report = run(&processes, &[]);
        assert_eq!(report.workspaces[&ws].agent_seconds, 3600.0);
    }

    #[test]
    fn activity_version_only_reflects_the_period() {
        let ws = Uuid::new_v4();
        let yesterday = Utc.with_ymd_and_hms(2026, 10, 19, 15, 0, 0).unwrap();
        let processes = [
            agent_run(ws, yesterday, yesterday + Duration::minutes(5), None),
            agent_run(ws, at(9, 0), at(9, 30), None),
        ];
        let versions = period_activity_versions(&processes, at(0, 0), at(0, 0) + Duration::days(1));
        assert_eq!(versions[&ws], at(9, 30));

        let none = period_activity_versions(&processes, at(12, 0), at(13, 0));
        assert!(none.is_empty());
    }
}
