use std::collections::HashSet;

use serde_json::Value;

/// Update the set of outstanding background `task_id`s from a single stream line.
///
/// Background work (both `run_in_background` bash and async subagents) surfaces as
/// `system` messages: `task_started` when it launches and `task_notification` when it
/// reaches a terminal state. `task_updated` is progress only and is ignored. Keying by
/// `task_id` keeps concurrent tasks independent and is self-correcting for foreground
/// tasks that start and finish within one turn.
pub(crate) fn apply_task_event(outstanding: &mut HashSet<String>, line: &Value) {
    if line.get("type").and_then(|t| t.as_str()) != Some("system") {
        return;
    }
    let Some(task_id) = line.get("task_id").and_then(|t| t.as_str()) else {
        return;
    };
    match line.get("subtype").and_then(|s| s.as_str()) {
        Some("task_started") => {
            outstanding.insert(task_id.to_string());
        }
        Some("task_notification") => {
            outstanding.remove(task_id);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::*;

    #[test]
    fn task_started_inserts_and_notification_removes() {
        let mut out: HashSet<String> = HashSet::new();
        apply_task_event(
            &mut out,
            &json!({"type":"system","subtype":"task_started","task_id":"t1"}),
        );
        assert_eq!(out.len(), 1);
        // progress event does not change the set
        apply_task_event(
            &mut out,
            &json!({"type":"system","subtype":"task_updated","task_id":"t1"}),
        );
        assert_eq!(out.len(), 1);
        apply_task_event(
            &mut out,
            &json!({"type":"system","subtype":"task_notification","task_id":"t1","status":"completed"}),
        );
        assert!(out.is_empty());
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
}
