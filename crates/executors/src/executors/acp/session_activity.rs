//! Reads Kimi Code's per-session wire log to tell when the agent is idle.
//!
//! Kimi settles a prompt that arrives during one of its automatic
//! background-task notification turns with an empty `end_turn`; the prompt is
//! queued and runs later without any of its events reaching the ACP client.
//! That hidden turn is invisible over ACP, but Kimi appends every turn to
//! `<KIMI_CODE_HOME>/sessions/<workspace>/<session-id>/agents/main/wire.jsonl`
//! as `turn.prompt` / `turn.ended` records. Reading that file lets the harness
//! wait for the hidden turn instead of probing it with prompts, which would
//! themselves be queued as further hidden turns.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// How often the wire log is polled while waiting for idle.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// The log must stay closed this long before a prompt is sent, so a turn
/// that is just starting is not raced.
const IDLE_STABLE_FOR: Duration = Duration::from_secs(1);

/// How a wait for session idleness ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleWait {
    /// No turn is executing and none started during the stability window.
    Idle,
    /// The wait timed out with a turn still open.
    TimedOut,
    /// The wire log is missing, unreadable, or the agent process is gone.
    Unavailable,
    /// The execution was cancelled while waiting.
    Cancelled,
}

/// A Kimi session's wire log, used to see turns ACP does not forward.
#[derive(Debug, Clone)]
pub struct SessionActivity {
    home: PathBuf,
    session_id: String,
    cwd: PathBuf,
    /// Wire `time` values (ms since epoch) below this belong to a previous
    /// process; ignoring them keeps a killed turn from stalling a resume.
    since_ms: u128,
}

impl SessionActivity {
    pub fn new(
        home: impl Into<PathBuf>,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            home: home.into(),
            session_id: session_id.into(),
            cwd: cwd.into(),
            since_ms: now_ms(),
        }
    }

    /// Whether the session currently has an open turn. `None` when the wire
    /// log cannot be found or read.
    pub async fn is_idle(&self) -> Option<bool> {
        let path = self.wire_path()?;
        let text = tokio::fs::read_to_string(path).await.ok()?;
        Some(open_turns(&text, self.since_ms).is_empty())
    }

    /// Wait until no turn is open for `IDLE_STABLE_FOR`. `agent_alive` is
    /// polled so a dead process cannot stall the wait.
    pub async fn wait_until_idle(
        &self,
        cancel: &CancellationToken,
        timeout: Duration,
        mut agent_alive: impl FnMut() -> bool,
    ) -> IdleWait {
        let Some(path) = self.wire_path() else {
            return IdleWait::Unavailable;
        };
        let Ok(mut file) = tokio::fs::File::open(&path).await else {
            return IdleWait::Unavailable;
        };
        let mut state = WireState::default();
        let mut offset = 0u64;
        let mut pending = Vec::new();
        if !read_appended(
            &mut file,
            &mut offset,
            &mut pending,
            &mut state,
            self.since_ms,
        )
        .await
        {
            return IdleWait::Unavailable;
        }

        let deadline = tokio::time::Instant::now() + timeout;
        let mut idle_since = state.open.is_empty().then(tokio::time::Instant::now);
        loop {
            if cancel.is_cancelled() {
                return IdleWait::Cancelled;
            }
            if !agent_alive() {
                return IdleWait::Unavailable;
            }
            if tokio::time::Instant::now() >= deadline {
                return IdleWait::TimedOut;
            }
            tokio::select! {
                _ = cancel.cancelled() => return IdleWait::Cancelled,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }

            // Recover from rotation or truncation by reparsing from scratch.
            let len = file.metadata().await.map(|meta| meta.len()).unwrap_or(0);
            if len < offset {
                let Ok(fresh) = tokio::fs::File::open(&path).await else {
                    return IdleWait::Unavailable;
                };
                file = fresh;
                offset = 0;
                pending.clear();
                state = WireState::default();
            }
            if !read_appended(
                &mut file,
                &mut offset,
                &mut pending,
                &mut state,
                self.since_ms,
            )
            .await
            {
                return IdleWait::Unavailable;
            }

            if state.open.is_empty() {
                match idle_since {
                    Some(since) if since.elapsed() >= IDLE_STABLE_FOR => return IdleWait::Idle,
                    Some(_) => {}
                    None => idle_since = Some(tokio::time::Instant::now()),
                }
            } else {
                idle_since = None;
            }
        }
    }

    fn wire_path(&self) -> Option<PathBuf> {
        locate_kimi_wire(&self.home, &self.session_id, &self.cwd)
    }
}

/// Find `<home>/sessions/<workspace>/<session-id>/agents/main/wire.jsonl`.
///
/// The session id is unique per Kimi session, so parallel sessions each
/// resolve to their own file. If the same id exists under more than one
/// workspace, the session's `state.json` cwd picks the right one; ambiguity
/// yields `None` so the caller falls back to timed retries instead of
/// watching the wrong session.
pub fn locate_kimi_wire(home: &Path, session_id: &str, cwd: &Path) -> Option<PathBuf> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return None;
    }
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(home.join("sessions")).ok()?.flatten() {
        let candidate = entry
            .path()
            .join(session_id)
            .join("agents")
            .join("main")
            .join("wire.jsonl");
        if candidate.is_file() {
            matches.push(candidate);
        }
    }
    match matches.len() {
        0 => None,
        1 => matches.pop(),
        _ => {
            let mut matching: Vec<PathBuf> = matches
                .into_iter()
                .filter(|wire| {
                    session_cwd(wire).is_some_and(|session_cwd| same_path(&session_cwd, cwd))
                })
                .collect();
            (matching.len() == 1).then(|| matching.remove(0))
        }
    }
}

fn session_cwd(wire: &Path) -> Option<PathBuf> {
    let session = wire.parent()?.parent()?.parent()?;
    let text = std::fs::read_to_string(session.join("state.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("cwd")?.as_str().map(PathBuf::from)
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[derive(Debug, Default)]
struct WireState {
    open: BTreeSet<u64>,
}

impl WireState {
    fn apply_line(&mut self, line: &str, since_ms: u128) {
        let Ok(event) = serde_json::from_str::<WireEvent>(line) else {
            return;
        };
        let Some(turn_id) = event.turn_id else {
            return;
        };
        match event.kind.as_str() {
            "turn.prompt" => {
                if event.time.is_none_or(|time| u128::from(time) >= since_ms) {
                    self.open.insert(turn_id);
                }
            }
            "turn.ended" | "turn.cancel" | "agent.turn.ended" => {
                self.open.remove(&turn_id);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "turnId")]
    turn_id: Option<u64>,
    time: Option<u64>,
}

fn open_turns(text: &str, since_ms: u128) -> BTreeSet<u64> {
    let mut state = WireState::default();
    for line in text.lines() {
        state.apply_line(line, since_ms);
    }
    state.open
}

/// Append newly written bytes to `pending`, applying every complete line.
async fn read_appended(
    file: &mut tokio::fs::File,
    offset: &mut u64,
    pending: &mut Vec<u8>,
    state: &mut WireState,
    since_ms: u128,
) -> bool {
    let mut chunk = Vec::new();
    if file.read_to_end(&mut chunk).await.is_err() {
        return false;
    }
    if chunk.is_empty() {
        return true;
    }
    *offset += chunk.len() as u64;
    pending.extend_from_slice(&chunk);
    let mut start = 0;
    while let Some(position) = pending[start..].iter().position(|byte| *byte == b'\n') {
        let end = start + position;
        if let Ok(line) = std::str::from_utf8(&pending[start..end]) {
            state.apply_line(line, since_ms);
        }
        start = end + 1;
    }
    pending.drain(..start);
    true
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(home: &Path, workspace: &str, session_id: &str, cwd: &Path, wire: &str) {
        let session = home.join("sessions").join(workspace).join(session_id);
        std::fs::create_dir_all(session.join("agents").join("main")).unwrap();
        std::fs::write(
            session.join("state.json"),
            serde_json::json!({ "cwd": cwd.to_string_lossy() }).to_string(),
        )
        .unwrap();
        std::fs::write(session.join("agents").join("main").join("wire.jsonl"), wire).unwrap();
    }

    fn prompt(turn_id: u64, time: u128) -> String {
        serde_json::json!({
            "type": "turn.prompt",
            "turnId": turn_id,
            "time": time,
        })
        .to_string()
            + "\n"
    }

    fn ended(turn_id: u64, time: u128) -> String {
        serde_json::json!({
            "type": "turn.ended",
            "turnId": turn_id,
            "reason": "completed",
            "time": time,
        })
        .to_string()
            + "\n"
    }

    #[test]
    fn parallel_sessions_resolve_to_their_own_wire_logs() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let cwd = temp.path().join("worktree");
        write_session(home, "wd_a_1", "session_aaa", &cwd, "");
        write_session(home, "wd_b_2", "session_bbb", &cwd, "");

        let a = locate_kimi_wire(home, "session_aaa", &cwd).unwrap();
        let b = locate_kimi_wire(home, "session_bbb", &cwd).unwrap();
        assert!(a.ends_with("session_aaa/agents/main/wire.jsonl"));
        assert!(b.ends_with("session_bbb/agents/main/wire.jsonl"));
        assert_ne!(a, b);
    }

    #[test]
    fn duplicate_session_id_is_disambiguated_by_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let cwd_a = temp.path().join("a");
        let cwd_b = temp.path().join("b");
        write_session(home, "wd_a_1", "session_same", &cwd_a, "");
        write_session(home, "wd_b_2", "session_same", &cwd_b, "");

        let found = locate_kimi_wire(home, "session_same", &cwd_b).unwrap();
        assert!(found.starts_with(home.join("sessions").join("wd_b_2")));
    }

    #[test]
    fn ambiguous_or_unknown_sessions_are_not_guessed() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let cwd = temp.path().join("worktree");
        write_session(home, "wd_a_1", "session_same", &cwd, "");
        write_session(home, "wd_b_2", "session_same", &cwd, "");

        assert!(locate_kimi_wire(home, "session_same", &cwd).is_none());
        assert!(locate_kimi_wire(home, "session_missing", &cwd).is_none());
        assert!(locate_kimi_wire(home, "../session_aaa", &cwd).is_none());
    }

    #[test]
    fn turns_open_and_close_in_order() {
        let text = prompt(0, 10) + &ended(0, 20) + &prompt(1, 30);
        assert_eq!(open_turns(&text, 0), BTreeSet::from([1]));
        let cancelled =
            prompt(1, 30) + &serde_json::json!({"type": "turn.cancel", "turnId": 1}).to_string();
        assert!(open_turns(&cancelled, 0).is_empty());
    }

    #[test]
    fn turns_from_a_previous_process_are_ignored() {
        // The old process was killed mid-turn; the new one must not wait on it.
        let old = prompt(4, 100);
        assert!(open_turns(&old, 1_000).is_empty());
        let current = prompt(4, 1_000);
        assert_eq!(open_turns(&current, 1_000), BTreeSet::from([4]));
    }

    fn write_activity(home: &Path, cwd: &Path) -> SessionActivity {
        let activity = SessionActivity::new(home, "session_live", cwd);
        write_session(home, "wd_live_1", "session_live", cwd, "");
        activity
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_an_open_turn_to_finish() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let cwd = temp.path().join("worktree");
        let activity = write_activity(&home, &cwd);
        let wire = locate_kimi_wire(&home, "session_live", &cwd).unwrap();
        let start_ms = now_ms() + 1_000;
        std::fs::write(&wire, prompt(1, start_ms)).unwrap();

        let writer = wire.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(writer)
                .unwrap();
            use std::io::Write;
            file.write_all(ended(1, start_ms).as_bytes()).unwrap();
        });

        let cancel = CancellationToken::new();
        let started = tokio::time::Instant::now();
        let wait = activity
            .wait_until_idle(&cancel, Duration::from_secs(30), || true)
            .await;
        assert_eq!(wait, IdleWait::Idle);
        assert!(started.elapsed() >= Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn idle_session_returns_after_the_stability_window() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let cwd = temp.path().join("worktree");
        let activity = write_activity(&home, &cwd);
        let wire = locate_kimi_wire(&home, "session_live", &cwd).unwrap();
        std::fs::write(&wire, ended(1, now_ms() + 1_000)).unwrap();

        let cancel = CancellationToken::new();
        assert_eq!(
            activity
                .wait_until_idle(&cancel, Duration::from_secs(30), || true)
                .await,
            IdleWait::Idle
        );
    }

    #[tokio::test(start_paused = true)]
    async fn busy_session_times_out_or_cancels() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let cwd = temp.path().join("worktree");
        let activity = write_activity(&home, &cwd);
        let wire = locate_kimi_wire(&home, "session_live", &cwd).unwrap();
        std::fs::write(&wire, prompt(1, now_ms() + 1_000)).unwrap();

        let cancel = CancellationToken::new();
        assert_eq!(
            activity
                .wait_until_idle(&cancel, Duration::from_secs(5), || true)
                .await,
            IdleWait::TimedOut
        );

        let cancel = CancellationToken::new();
        let cancel_after = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            cancel_after.cancel();
        });
        assert_eq!(
            activity
                .wait_until_idle(&cancel, Duration::from_secs(30), || true)
                .await,
            IdleWait::Cancelled
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_dead_agent_ends_the_wait() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let cwd = temp.path().join("worktree");
        let activity = write_activity(&home, &cwd);
        let wire = locate_kimi_wire(&home, "session_live", &cwd).unwrap();
        std::fs::write(&wire, prompt(1, now_ms() + 1_000)).unwrap();

        assert_eq!(
            activity
                .wait_until_idle(&CancellationToken::new(), Duration::from_secs(30), || false)
                .await,
            IdleWait::Unavailable
        );
    }

    #[tokio::test(start_paused = true)]
    async fn missing_wire_log_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let activity = SessionActivity::new(temp.path(), "session_missing", temp.path());
        assert_eq!(
            activity
                .wait_until_idle(&CancellationToken::new(), Duration::from_secs(5), || true)
                .await,
            IdleWait::Unavailable
        );
        assert_eq!(activity.is_idle().await, None);
    }
}
