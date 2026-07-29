use std::collections::HashSet;

use serde_json::Value;

/// What a stream line meant for background-task bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskEvent {
    /// Not a background task lifecycle event.
    Ignored,
    /// A background task started in this session.
    Started,
    /// A background task this session started reached a terminal state.
    Finished,
    /// A terminal notification for a task this session never started. Claude
    /// replays these on resume when an earlier process was torn down while the
    /// task was still outstanding. The replay is queued *ahead* of our prompt and
    /// is answered by its own zero-turn `Result` (see [`on_result`]).
    Replayed,
}

/// Update the set of outstanding background `task_id`s from a single stream line.
///
/// Background work (both `run_in_background` bash and async subagents) surfaces as
/// `system` messages: `task_started` when it launches and `task_notification` when it
/// reaches a terminal state. `task_updated` is progress only and is ignored. Keying by
/// `task_id` keeps concurrent tasks independent and is self-correcting for foreground
/// tasks that start and finish within one turn.
pub(crate) fn apply_task_event(outstanding: &mut HashSet<String>, line: &Value) -> TaskEvent {
    if line.get("type").and_then(|t| t.as_str()) != Some("system") {
        return TaskEvent::Ignored;
    }
    let Some(task_id) = line.get("task_id").and_then(|t| t.as_str()) else {
        return TaskEvent::Ignored;
    };
    match line.get("subtype").and_then(|s| s.as_str()) {
        Some("task_started") => {
            outstanding.insert(task_id.to_string());
            TaskEvent::Started
        }
        // A notification for a task we never saw start belongs to a previous
        // process, so it cannot be accounted for by our outstanding set.
        Some("task_notification") => {
            if outstanding.remove(task_id) {
                TaskEvent::Finished
            } else {
                TaskEvent::Replayed
            }
        }
        _ => TaskEvent::Ignored,
    }
}

/// What the read loop should do with a `Result` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnAction {
    /// End the read loop: this `Result` closed the turn.
    End,
    /// Keep reading: a background task this session started is still running and
    /// Claude auto-continues the turn once it completes.
    AwaitBackground,
    /// Keep reading: this `Result` only answered a replayed notification and our
    /// prompt has not been processed yet.
    SkipReplayed,
}

/// A `Result` that answered nothing but a replayed notification: the run consumed
/// no user turn (`num_turns == 0`) and was triggered by the notification rather
/// than by a prompt. A genuine background continuation carries the same `origin`
/// but a non-zero turn count, so both fields are required to tell them apart.
fn is_replay_result(result: &Value) -> bool {
    result.get("num_turns").and_then(Value::as_u64) == Some(0)
        && result
            .get("origin")
            .and_then(|origin| origin.get("kind"))
            .and_then(Value::as_str)
            == Some("task-notification")
}

/// Decide whether a `Result` ends the turn.
///
/// Normally the turn only ends once no background tasks are outstanding, so the
/// session stays alive across `run_in_background` work and auto-continues. Two
/// cases override that:
///
/// - Once cancellation has been requested (`interrupted`), the user is stopping the
///   session, so any `Result` ends it promptly rather than waiting on a background
///   task that will be force-killed anyway.
/// - When a previous process was torn down with a background task still running,
///   Claude replays a terminal notification for it at the head of the next resume.
///   That replay is answered by its own zero-turn `Result` *before* our queued
///   prompt is dequeued, so ending on it would tear the CLI down mid-answer and
///   drop the reply. `replayed` counts notifications still awaiting such a result.
pub(crate) fn on_result(
    interrupted: bool,
    outstanding: &HashSet<String>,
    replayed: usize,
    result: &Value,
) -> TurnAction {
    if interrupted {
        return TurnAction::End;
    }
    if replayed > 0 && is_replay_result(result) {
        return TurnAction::SkipReplayed;
    }
    if outstanding.is_empty() {
        TurnAction::End
    } else {
        TurnAction::AwaitBackground
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::*;

    fn outstanding(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn task_started_inserts_and_notification_removes() {
        let mut out: HashSet<String> = HashSet::new();
        assert_eq!(
            apply_task_event(
                &mut out,
                &json!({"type":"system","subtype":"task_started","task_id":"t1"}),
            ),
            TaskEvent::Started
        );
        assert_eq!(out.len(), 1);
        // progress event does not change the set
        assert_eq!(
            apply_task_event(
                &mut out,
                &json!({"type":"system","subtype":"task_updated","task_id":"t1"}),
            ),
            TaskEvent::Ignored
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            apply_task_event(
                &mut out,
                &json!({"type":"system","subtype":"task_notification","task_id":"t1","status":"completed"}),
            ),
            TaskEvent::Finished
        );
        assert!(out.is_empty());
    }

    #[test]
    fn notification_for_a_task_we_never_started_is_replayed() {
        let mut out = outstanding(&["mine"]);
        assert_eq!(
            apply_task_event(
                &mut out,
                &json!({"type":"system","subtype":"task_notification","task_id":"theirs","status":"stopped"}),
            ),
            TaskEvent::Replayed
        );
        // a replay must not disturb the tasks this session owns
        assert_eq!(out, outstanding(&["mine"]));
    }

    #[test]
    fn unrelated_lines_are_ignored() {
        let mut out: HashSet<String> = HashSet::new();
        apply_task_event(&mut out, &json!({"type":"result","subtype":"success"}));
        apply_task_event(
            &mut out,
            &json!({"type":"assistant","message":{"role":"assistant","content":[]}}),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn full_captured_stream_returns_to_empty_with_late_completion() {
        // The fixture is ordered: bash -> task_started -> result#1 -> task_updated
        // -> task_notification -> result#2. The set must be NON-empty at result#1
        // and empty only after the task_notification.
        let raw = include_str!("fixtures/bg_bash_stream.jsonl");
        let mut out: HashSet<String> = HashSet::new();
        let mut seen_first_result = false;
        let mut nonempty_at_first_result = false;
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).unwrap();
            if v.get("type").and_then(|t| t.as_str()) == Some("result") && !seen_first_result {
                seen_first_result = true;
                nonempty_at_first_result = !out.is_empty();
            }
            apply_task_event(&mut out, &v);
        }
        assert!(
            nonempty_at_first_result,
            "background task must still be outstanding at the first result"
        );
        assert!(
            out.is_empty(),
            "set must be empty after the completion notification"
        );
    }

    #[test]
    fn result_ends_turn_only_when_nothing_is_outstanding() {
        let result = json!({"type":"result","subtype":"success","num_turns":2});
        assert_eq!(
            on_result(false, &HashSet::new(), 0, &result),
            TurnAction::End
        );
        assert_eq!(
            on_result(false, &outstanding(&["t1"]), 0, &result),
            TurnAction::AwaitBackground
        );
    }

    #[test]
    fn interrupt_ends_turn_even_with_outstanding_tasks() {
        // A stop request must end the session on the next Result rather than
        // waiting on a background task that will be force-killed anyway.
        let result = json!({"type":"result","subtype":"success","num_turns":0,
                            "origin":{"kind":"task-notification"}});
        assert_eq!(
            on_result(true, &outstanding(&["t1"]), 1, &result),
            TurnAction::End
        );
    }

    #[test]
    fn genuine_background_continuation_still_ends_the_turn() {
        // result#2 of the captured background stream: same `task-notification`
        // origin as a replay, but it consumed real turns, so it is our answer.
        let raw = include_str!("fixtures/bg_bash_stream.jsonl");
        let final_result: Value = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("result"))
            .next_back()
            .expect("fixture has a final result");
        assert_eq!(
            final_result.get("origin").and_then(|o| o.get("kind")),
            Some(&json!("task-notification")),
            "fixture precondition: continuation carries the notification origin"
        );
        assert_eq!(
            on_result(false, &HashSet::new(), 0, &final_result),
            TurnAction::End
        );
    }

    #[test]
    fn replayed_notification_result_does_not_end_the_turn() {
        // Captured from a resume whose previous process was killed with a
        // `run_in_background` bash task still running: the replayed notification
        // and its empty result arrive before the queued prompt is dequeued, and
        // ending there tore the CLI down mid-answer.
        let raw = include_str!("fixtures/stale_bg_notification_resume.jsonl");
        let mut out: HashSet<String> = HashSet::new();
        let mut replayed = 0usize;
        let mut seen_replays = 0usize;
        let mut ended_at = None;

        for (idx, line) in raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, l)| (i, serde_json::from_str::<Value>(l).unwrap()))
        {
            if apply_task_event(&mut out, &line) == TaskEvent::Replayed {
                replayed += 1;
                seen_replays += 1;
            }
            if line.get("type").and_then(|t| t.as_str()) != Some("result") {
                continue;
            }
            match on_result(false, &out, replayed, &line) {
                TurnAction::SkipReplayed => replayed -= 1,
                TurnAction::AwaitBackground => {}
                TurnAction::End => {
                    ended_at = Some(idx);
                    break;
                }
            }
        }

        assert_eq!(
            seen_replays, 1,
            "fixture precondition: one replayed notification"
        );
        assert_eq!(replayed, 0, "the replay must be consumed by its own result");
        assert_eq!(
            ended_at,
            Some(3),
            "turn must end on the real result (line 3), not the replay's empty one (line 1)"
        );
    }
}
