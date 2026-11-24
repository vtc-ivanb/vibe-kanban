//! Keeps a turn going while the agent's background tasks are still running.
//!
//! When an ACP turn ends, the container kills the agent's whole process
//! group, including commands the agent started with `run_in_background`.
//! Kimi Code also reacts to a finished background task by starting a turn on
//! its own, and its ACP server never forwards events from turns the client did
//! not start. So instead of letting the turn end, the harness asks the agent to
//! wait for its tasks in the foreground: the completion then lands inside a
//! turn the client started and stays visible.
//!
//! Tool updates are read from the agent's stdout before the ACP connection
//! parses them, so every update sent ahead of a prompt response is recorded by
//! the time that response resolves.
//!
//! Known gap: a task that exits (or is stopped with `TaskStop`) around the end
//! of a turn makes Kimi start its own (invisible) turn to deliver the
//! notification. A prompt that arrives as that turn starts comes back as an
//! empty `end_turn`, and prompts sent while it runs are queued behind it and
//! settle empty too. The harness resends the prompt (see
//! `UNANSWERED_PROMPT_RETRY_DELAYS` in `harness.rs`) with pauses spread across
//! the range of a full turn so the agent can report afterwards, but the
//! automatic turn's own output is never shown.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::{Arc, Mutex},
};

use agent_client_protocol as proto;
use serde::Deserialize;

/// Re-prompts in a row in which the agent made no tool call before giving up.
const MAX_IDLE_ROUNDS: u32 = 2;
/// Upper bound on wait re-prompts for one execution.
const MAX_WAIT_ROUNDS: u32 = 20;
/// One wait chunk; Kimi caps foreground commands at 300s.
const WAIT_CHUNK_SECS: u32 = 275;
const WAIT_POLL_SECS: u32 = 5;
/// Partial stdout lines longer than this are not inspected.
const MAX_OBSERVED_LINE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTask {
    pub task_id: String,
    pub pid: u32,
}

/// Parse Kimi's result for a command started in the background:
///
/// ```text
/// task_id: bash-c0vukvko
/// pid: 54889
/// description: ...
/// status: running
/// ```
pub fn parse_started_task(text: &str) -> Option<BackgroundTask> {
    let mut task_id = None;
    let mut pid = None;
    let mut running = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "task_id" if task_id.is_none() && !value.is_empty() => {
                task_id = Some(value.to_string())
            }
            "pid" if pid.is_none() => pid = value.parse().ok(),
            "status" => running = value == "running",
            _ => {}
        }
    }
    match (task_id, pid, running) {
        (Some(task_id), Some(pid), true) => Some(BackgroundTask { task_id, pid }),
        _ => None,
    }
}

fn tool_update_texts(update: &proto::ToolCallUpdate) -> Vec<&str> {
    let mut texts: Vec<&str> = update
        .fields
        .content
        .iter()
        .flatten()
        .filter_map(|content| match content {
            proto::ToolCallContent::Content(proto::Content {
                content: proto::ContentBlock::Text(text),
                ..
            }) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    if let Some(serde_json::Value::String(raw)) = &update.fields.raw_output {
        texts.push(raw);
    }
    texts
}

#[derive(Debug, Default)]
struct TrackerState {
    tasks: BTreeMap<String, u32>,
    tool_calls_this_turn: usize,
}

/// Records the background tasks an agent starts, fed from session updates.
#[derive(Debug, Clone, Default)]
pub struct BackgroundTaskTracker {
    state: Arc<Mutex<TrackerState>>,
}

impl BackgroundTaskTracker {
    /// Inspect one JSON-RPC line sent by the agent.
    pub fn observe_line(&self, line: &[u8]) {
        // Cheap pre-filter: only tool call notifications matter.
        if !contains(line, b"session/update") || !contains(line, b"tool_call") {
            return;
        }
        let Ok(message) = serde_json::from_slice::<serde_json::Value>(line) else {
            return;
        };
        if message.get("method").and_then(|m| m.as_str()) != Some("session/update") {
            return;
        }
        let Some(Ok(notification)) = message
            .get("params")
            .map(|params| proto::SessionNotification::deserialize(params))
        else {
            return;
        };
        match notification.update {
            proto::SessionUpdate::ToolCall(_) => self.observe_tool_call(),
            proto::SessionUpdate::ToolCallUpdate(update) => self.observe_tool_update(&update),
            _ => {}
        }
    }

    fn observe_tool_call(&self) {
        self.state.lock().unwrap().tool_calls_this_turn += 1;
    }

    fn observe_tool_update(&self, update: &proto::ToolCallUpdate) {
        if update.fields.status != Some(proto::ToolCallStatus::Completed) {
            return;
        }
        let started: Vec<_> = tool_update_texts(update)
            .into_iter()
            .filter_map(parse_started_task)
            .collect();
        if started.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        for task in started {
            state.tasks.insert(task.task_id, task.pid);
        }
    }

    /// Number of tool calls since the last call, resetting the count.
    pub fn take_turn_tool_calls(&self) -> usize {
        std::mem::take(&mut self.state.lock().unwrap().tool_calls_this_turn)
    }

    /// Tasks whose process is still alive; finished ones are forgotten.
    pub fn live_tasks(&self, mut is_alive: impl FnMut(u32) -> bool) -> Vec<BackgroundTask> {
        let mut state = self.state.lock().unwrap();
        state.tasks.retain(|_, pid| is_alive(*pid));
        state
            .tasks
            .iter()
            .map(|(task_id, pid)| BackgroundTask {
                task_id: task_id.clone(),
                pid: *pid,
            })
            .collect()
    }
}

/// Stop surviving task groups, which Kimi detaches from the agent's group.
pub async fn stop_background_tasks(tracker: &BackgroundTaskTracker, agent_pid: Option<u32>) {
    stop_background_tasks_with(
        tracker,
        |pid| {
            agent_pid.is_some_and(|agent| workspace_utils::process::is_live_descendant(pid, agent))
        },
        |pids| async move {
            let _ = workspace_utils::process::kill_process_groups(&pids).await;
        },
    )
    .await;
}

async fn stop_background_tasks_with<Fut>(
    tracker: &BackgroundTaskTracker,
    mut is_alive: impl FnMut(u32) -> bool,
    mut kill: impl FnMut(Vec<u32>) -> Fut,
) where
    Fut: Future<Output = ()>,
{
    let mut handled = BTreeSet::new();
    loop {
        let tasks = tracker
            .live_tasks(&mut is_alive)
            .into_iter()
            .filter(|task| handled.insert((task.task_id.clone(), task.pid)))
            .collect::<Vec<_>>();
        if tasks.is_empty() {
            break;
        }
        kill(tasks.into_iter().map(|task| task.pid).collect()).await;
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Splits the agent's stdout into lines for a [`BackgroundTaskTracker`].
#[derive(Debug)]
pub struct StdoutObserver {
    tracker: BackgroundTaskTracker,
    partial: Vec<u8>,
    overflowed: bool,
}

impl StdoutObserver {
    pub fn new(tracker: BackgroundTaskTracker) -> Self {
        Self {
            tracker,
            partial: Vec::new(),
            overflowed: false,
        }
    }

    /// Feed a chunk; call before forwarding it to the ACP connection.
    pub fn observe(&mut self, mut chunk: &[u8]) {
        while let Some(newline) = chunk.iter().position(|b| *b == b'\n') {
            let (head, rest) = chunk.split_at(newline);
            if !self.overflowed {
                if self.partial.is_empty() {
                    self.tracker.observe_line(head);
                } else {
                    self.partial.extend_from_slice(head);
                    self.tracker.observe_line(&self.partial);
                }
            }
            self.partial.clear();
            self.overflowed = false;
            chunk = &rest[1..];
        }
        if self.overflowed {
            return;
        }
        if self.partial.len() + chunk.len() > MAX_OBSERVED_LINE_BYTES {
            self.partial = Vec::new();
            self.overflowed = true;
        } else {
            self.partial.extend_from_slice(chunk);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AfterTurn {
    /// No background work left; the execution can finish.
    Finish,
    /// Ask the agent to wait for these tasks.
    Wait(Vec<BackgroundTask>),
    /// The agent keeps ending its turn without waiting; let the tasks die.
    GiveUp(Vec<BackgroundTask>),
}

/// Decides what happens when a turn ends while tasks may still be running.
#[derive(Debug, Default)]
pub struct WaitPolicy {
    rounds: u32,
    idle_rounds: u32,
}

impl WaitPolicy {
    pub fn after_turn(&mut self, live: Vec<BackgroundTask>, turn_tool_calls: usize) -> AfterTurn {
        if live.is_empty() {
            return AfterTurn::Finish;
        }
        if self.rounds > 0 {
            if turn_tool_calls == 0 {
                self.idle_rounds += 1;
            } else {
                self.idle_rounds = 0;
            }
        }
        if self.idle_rounds >= MAX_IDLE_ROUNDS || self.rounds >= MAX_WAIT_ROUNDS {
            return AfterTurn::GiveUp(live);
        }
        self.rounds += 1;
        AfterTurn::Wait(live)
    }
}

fn describe(tasks: &[BackgroundTask]) -> String {
    tasks
        .iter()
        .map(|task| format!("{} (pid {})", task.task_id, task.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn wait_prompt(tasks: &[BackgroundTask]) -> String {
    let pids = tasks
        .iter()
        .map(|task| task.pid.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let polls = WAIT_CHUNK_SECS / WAIT_POLL_SECS;
    // One time budget shared by all pids, so the command stays under the cap.
    format!(
        "[vibe-kanban] Your turn ended while background task(s) are still running: {tasks}. \
         This host stops the agent process, and kills its background tasks, as soon as your \
         turn ends, so you must not end your turn while they run. Wait for them in the \
         foreground (run_in_background=false, timeout=300) with this bounded wait, repeating \
         it as many times as needed until all have exited: \
         `for i in $(seq {polls}); do alive=; for p in {pids}; do kill -0 $p 2>/dev/null && \
         alive=1; done; [ -z \"$alive\" ] && break; sleep {WAIT_POLL_SECS}; done; \
         for p in {pids}; do kill -0 $p 2>/dev/null && echo \"still running: $p\"; done; true`. \
         Then read their results and continue with your task.",
        tasks = describe(tasks),
    )
}

pub fn waiting_notice(tasks: &[BackgroundTask]) -> String {
    format!(
        "Background task(s) still running: {}. Asking the agent to wait for them before \
         finishing, because they are stopped when the turn ends.",
        describe(tasks)
    )
}

pub fn give_up_notice(tasks: &[BackgroundTask]) -> String {
    format!(
        "The agent ended its turn without waiting for background task(s) {}. They are stopped \
         now that the turn is over.",
        describe(tasks)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTED: &str = "task_id: bash-c0vukvko\npid: 54889\ndescription: Verify build\n\
        status: running\nautomatic_notification: true\nnext_step: The completion arrives \
        automatically in a later turn";

    fn task(task_id: &str, pid: u32) -> BackgroundTask {
        BackgroundTask {
            task_id: task_id.to_string(),
            pid,
        }
    }

    fn update(status: proto::ToolCallStatus, text: &str) -> proto::ToolCallUpdate {
        proto::ToolCallUpdate::new(
            "0:tool_1",
            proto::ToolCallUpdateFields::new()
                .status(status)
                .content(vec![proto::ToolCallContent::from(text.to_string())]),
        )
    }

    #[test]
    fn parses_started_background_command() {
        assert_eq!(
            parse_started_task(STARTED),
            Some(task("bash-c0vukvko", 54889))
        );
    }

    #[test]
    fn ignores_finished_tasks_and_subagents_without_pid() {
        assert_eq!(
            parse_started_task("task_id: bash-1\npid: 7\nstatus: completed"),
            None
        );
        assert_eq!(
            parse_started_task("task_id: agent-1\nstatus: running\nagent_id: a1"),
            None
        );
        assert_eq!(parse_started_task("Command executed successfully."), None);
    }

    #[test]
    fn tracker_records_only_completed_tool_results() {
        let tracker = BackgroundTaskTracker::default();
        tracker.observe_tool_update(&update(proto::ToolCallStatus::InProgress, STARTED));
        assert!(tracker.live_tasks(|_| true).is_empty());

        tracker.observe_tool_update(&update(proto::ToolCallStatus::Completed, STARTED));
        assert_eq!(
            tracker.live_tasks(|_| true),
            vec![task("bash-c0vukvko", 54889)]
        );
    }

    #[test]
    fn tracker_forgets_exited_tasks() {
        let tracker = BackgroundTaskTracker::default();
        tracker.observe_tool_update(&update(proto::ToolCallStatus::Completed, STARTED));
        assert!(tracker.live_tasks(|_| false).is_empty());
        assert!(tracker.live_tasks(|_| true).is_empty());
    }

    #[test]
    fn tracker_counts_tool_calls_per_turn() {
        let tracker = BackgroundTaskTracker::default();
        tracker.observe_tool_call();
        tracker.observe_tool_call();
        assert_eq!(tracker.take_turn_tool_calls(), 2);
        assert_eq!(tracker.take_turn_tool_calls(), 0);
    }

    fn notification_line(status: &str) -> String {
        let update = serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "0:tool_1",
            "status": status,
            "content": [{"type": "content", "content": {"type": "text", "text": STARTED}}],
        });
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "s1", "update": update},
        });
        format!("{message}\n")
    }

    #[test]
    fn observer_reads_tool_results_from_raw_stdout() {
        let tracker = BackgroundTaskTracker::default();
        let mut observer = StdoutObserver::new(tracker.clone());
        let tool_call = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"0:tool_1","title":"Bash"}}}"#;
        let stream = format!(
            "{tool_call}\n{}{}{}",
            notification_line("in_progress"),
            notification_line("completed"),
            r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#,
        );
        // Split mid-line to check partial lines are reassembled.
        for chunk in stream.as_bytes().chunks(37) {
            observer.observe(chunk);
        }
        assert_eq!(
            tracker.live_tasks(|_| true),
            vec![task("bash-c0vukvko", 54889)]
        );
        assert_eq!(tracker.take_turn_tool_calls(), 1);
    }

    #[test]
    fn observer_skips_oversized_lines_and_recovers() {
        let tracker = BackgroundTaskTracker::default();
        let mut observer = StdoutObserver::new(tracker.clone());
        observer.observe(&vec![b'x'; MAX_OBSERVED_LINE_BYTES + 1]);
        observer.observe(b"tail of the huge line\n");
        observer.observe(notification_line("completed").as_bytes());
        assert_eq!(tracker.live_tasks(|_| true).len(), 1);
    }

    #[test]
    fn policy_finishes_when_nothing_runs() {
        assert_eq!(
            WaitPolicy::default().after_turn(vec![], 0),
            AfterTurn::Finish
        );
    }

    #[test]
    fn policy_waits_while_the_agent_keeps_waiting() {
        let mut policy = WaitPolicy::default();
        let live = vec![task("bash-1", 1)];
        // The first turn's tool calls don't count as waiting.
        assert_eq!(
            policy.after_turn(live.clone(), 0),
            AfterTurn::Wait(live.clone())
        );
        for _ in 0..5 {
            assert_eq!(
                policy.after_turn(live.clone(), 1),
                AfterTurn::Wait(live.clone())
            );
        }
        assert_eq!(policy.after_turn(vec![], 0), AfterTurn::Finish);
    }

    #[test]
    fn policy_gives_up_after_idle_rounds() {
        let mut policy = WaitPolicy::default();
        let live = vec![task("bash-1", 1)];
        assert_eq!(
            policy.after_turn(live.clone(), 3),
            AfterTurn::Wait(live.clone())
        );
        assert_eq!(
            policy.after_turn(live.clone(), 0),
            AfterTurn::Wait(live.clone())
        );
        assert_eq!(policy.after_turn(live.clone(), 0), AfterTurn::GiveUp(live));
    }

    #[test]
    fn policy_gives_up_after_max_rounds() {
        let mut policy = WaitPolicy::default();
        let live = vec![task("bash-1", 1)];
        for _ in 0..MAX_WAIT_ROUNDS {
            assert_eq!(
                policy.after_turn(live.clone(), 1),
                AfterTurn::Wait(live.clone())
            );
        }
        assert_eq!(policy.after_turn(live.clone(), 1), AfterTurn::GiveUp(live));
    }

    #[tokio::test]
    async fn stops_tasks_observed_during_cleanup() {
        let tracker = BackgroundTaskTracker::default();
        tracker.observe_tool_update(&update(
            proto::ToolCallStatus::Completed,
            "task_id: bash-initial\npid: 11\nstatus: running",
        ));
        let live = Arc::new(Mutex::new(BTreeSet::from([11])));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let live_for_check = live.clone();
        let tracker_for_kill = tracker.clone();
        let live_for_kill = live.clone();
        let calls_for_kill = calls.clone();

        stop_background_tasks_with(
            &tracker,
            move |pid| live_for_check.lock().unwrap().contains(&pid),
            move |pids| {
                let tracker = tracker_for_kill.clone();
                let live = live_for_kill.clone();
                let calls = calls_for_kill.clone();
                async move {
                    calls.lock().unwrap().push(pids.clone());
                    for pid in pids {
                        live.lock().unwrap().remove(&pid);
                    }
                    if calls.lock().unwrap().len() == 1 {
                        live.lock().unwrap().insert(22);
                        tracker.observe_tool_update(&update(
                            proto::ToolCallStatus::Completed,
                            "task_id: bash-late\npid: 22\nstatus: running",
                        ));
                    }
                }
            },
        )
        .await;

        assert_eq!(*calls.lock().unwrap(), vec![vec![11], vec![22]]);
        assert!(live.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stops_detached_background_process_groups() {
        use std::{os::unix::process::CommandExt, process::Stdio};

        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let started = format!("task_id: bash-detached\npid: {pid}\nstatus: running");
        let tracker = BackgroundTaskTracker::default();
        tracker.observe_tool_update(&update(proto::ToolCallStatus::Completed, &started));
        assert!(workspace_utils::process::is_live_descendant(
            pid,
            std::process::id()
        ));

        stop_background_tasks(&tracker, Some(std::process::id())).await;

        child.wait().unwrap();
        assert!(!workspace_utils::process::is_live_descendant(
            pid,
            std::process::id()
        ));
        assert!(tracker.live_tasks(|_| false).is_empty());
    }

    #[test]
    fn wait_prompt_names_every_task_and_pid() {
        let prompt = wait_prompt(&[task("bash-1", 11), task("bash-2", 22)]);
        assert!(prompt.contains("bash-1 (pid 11), bash-2 (pid 22)"));
        assert!(prompt.contains("for i in $(seq 55); do alive=; for p in 11 22; do"));
        assert!(prompt.contains("run_in_background=false"));
    }
}
