use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, RwLock},
};

use chrono::{DateTime, Utc};
use futures::{StreamExt, future};
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

use crate::{log_msg::LogMsg, stream_lines::LinesStreamExt};

// 100 MB Limit
const HISTORY_BYTES: usize = 100000 * 1024;

#[derive(Clone)]
struct StoredMsg {
    msg: LogMsg,
    bytes: usize,
}

struct Inner {
    history: VecDeque<StoredMsg>,
    total_bytes: usize,
}

/// Wall-clock times for conversation entries, keyed by entry index.
///
/// Normalized entries are recomputed from raw agent output every time a
/// conversation is read, so `Utc::now()` is only a truthful stamp while the
/// agent is actually running. During a live run we stamp and remember each
/// index; those stamps are persisted alongside the raw log and handed back via
/// [`MsgStore::set_entry_timestamps`] on replay, so a historical conversation
/// shows when it happened rather than when it was opened.
#[derive(Default)]
struct EntryClock {
    recorded: HashMap<usize, DateTime<Utc>>,
    /// Replaying a stored log: never invent a time we don't have a record of.
    replaying: bool,
}

pub struct MsgStore {
    inner: RwLock<Inner>,
    entry_clock: RwLock<EntryClock>,
    sender: broadcast::Sender<LogMsg>,
}

/// `/entries/3` -> `3`. Diff patches key by repo/file rather than index, so
/// anything non-numeric is deliberately not an entry.
pub fn parse_entry_index(path: &str) -> Option<usize> {
    path.strip_prefix("/entries/")?.parse().ok()
}

impl Default for MsgStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MsgStore {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(100000);
        Self {
            inner: RwLock::new(Inner {
                history: VecDeque::with_capacity(32),
                total_bytes: 0,
            }),
            entry_clock: RwLock::new(EntryClock::default()),
            sender,
        }
    }

    pub fn push(&self, msg: LogMsg) {
        let _ = self.sender.send(msg.clone()); // live listeners
        let bytes = msg.approx_bytes();

        let mut inner = self.inner.write().unwrap();
        while inner.total_bytes.saturating_add(bytes) > HISTORY_BYTES {
            if let Some(front) = inner.history.pop_front() {
                inner.total_bytes = inner.total_bytes.saturating_sub(front.bytes);
            } else {
                break;
            }
        }
        inner.history.push_back(StoredMsg { msg, bytes });
        inner.total_bytes = inner.total_bytes.saturating_add(bytes);
    }

    // Convenience
    pub fn push_stdout<S: Into<String>>(&self, s: S) {
        self.push(LogMsg::Stdout(s.into()));
    }

    pub fn push_patch(&self, patch: json_patch::Patch) {
        self.push(LogMsg::JsonPatch(self.stamp_entry_patch(patch)));
    }

    /// Replay stored entry times instead of stamping with the current clock.
    ///
    /// Entries absent from `map` are left unstamped — a log recorded before
    /// timestamps existed should show no time, not today's date.
    pub fn set_entry_timestamps(&self, map: HashMap<usize, DateTime<Utc>>) {
        let mut clock = self.entry_clock.write().unwrap();
        clock.recorded = map;
        clock.replaying = true;
    }

    /// Fill in the `timestamp` of any normalized entry the patch adds or replaces.
    ///
    /// Every executor builds entries with `timestamp: None`, so this is the one
    /// place conversation entries get a time. Patches that carry no normalized
    /// entry, and entries that already have a timestamp, pass through untouched.
    fn stamp_entry_patch(&self, patch: json_patch::Patch) -> json_patch::Patch {
        if !patch
            .0
            .iter()
            .any(|op| parse_entry_index(op.path().as_ref()).is_some())
        {
            return patch;
        }

        let Ok(mut value) = serde_json::to_value(&patch) else {
            return patch;
        };
        let Some(ops) = value.as_array_mut() else {
            return patch;
        };

        let mut stamped_any = false;
        for op in ops.iter_mut() {
            let Some(index) = op
                .get("path")
                .and_then(|p| p.as_str())
                .and_then(parse_entry_index)
            else {
                continue;
            };

            let Some(entry) = op
                .get_mut("value")
                .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("NORMALIZED_ENTRY"))
                .and_then(|v| v.get_mut("content"))
            else {
                continue;
            };

            // An executor that supplies its own time wins over ours.
            if !matches!(entry.get("timestamp"), None | Some(serde_json::Value::Null)) {
                continue;
            }

            let Some(ts) = self.entry_time(index) else {
                continue;
            };
            entry["timestamp"] = serde_json::Value::String(ts.to_rfc3339());
            stamped_any = true;
        }

        if !stamped_any {
            return patch;
        }
        serde_json::from_value(value).unwrap_or(patch)
    }

    /// The time entry `index` first appeared, minting one if this is a live run.
    fn entry_time(&self, index: usize) -> Option<DateTime<Utc>> {
        let mut clock = self.entry_clock.write().unwrap();
        if let Some(existing) = clock.recorded.get(&index) {
            return Some(*existing);
        }
        if clock.replaying {
            return None;
        }
        let now = Utc::now();
        clock.recorded.insert(index, now);
        Some(now)
    }

    pub fn push_session_id(&self, session_id: String) {
        self.push(LogMsg::SessionId(session_id));
    }

    pub fn push_message_id(&self, id: String) {
        self.push(LogMsg::MessageId(id));
    }

    pub fn push_finished(&self) {
        self.push(LogMsg::Finished);
    }

    pub fn get_receiver(&self) -> broadcast::Receiver<LogMsg> {
        self.sender.subscribe()
    }

    pub fn get_history(&self) -> Vec<LogMsg> {
        self.inner
            .read()
            .unwrap()
            .history
            .iter()
            .map(|s| s.msg.clone())
            .collect()
    }

    /// Number of buffered history messages (cheap; no clone). Diagnostic use.
    pub fn history_len(&self) -> usize {
        self.inner.read().unwrap().history.len()
    }

    /// Whether a `Finished` marker is present in history (cheap; no clone).
    /// If false while the owning process is no longer running, the stream this
    /// store backs will never terminate on its own. Diagnostic use.
    pub fn is_finished(&self) -> bool {
        self.inner
            .read()
            .unwrap()
            .history
            .iter()
            .any(|s| matches!(s.msg, LogMsg::Finished))
    }

    /// History then live, as `LogMsg`.
    pub fn history_plus_stream(
        &self,
    ) -> futures::stream::BoxStream<'static, Result<LogMsg, std::io::Error>> {
        let (history, rx) = (self.get_history(), self.get_receiver());

        let hist = futures::stream::iter(history.into_iter().map(Ok::<_, std::io::Error>));
        let live = BroadcastStream::new(rx).filter_map(|res| async move {
            match res {
                Ok(msg) => Some(Ok(msg)),
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    tracing::error!(
                        skipped = n,
                        "MsgStore broadcast lagged. {n} messages dropped for this subscriber"
                    );
                    None
                }
            }
        });

        Box::pin(hist.chain(live))
    }

    pub fn stdout_chunked_stream(
        &self,
    ) -> futures::stream::BoxStream<'static, Result<String, std::io::Error>> {
        self.history_plus_stream()
            .take_while(|res| future::ready(!matches!(res, Ok(LogMsg::Finished))))
            .filter_map(|res| async move {
                match res {
                    Ok(LogMsg::Stdout(s)) => Some(Ok(s)),
                    _ => None,
                }
            })
            .boxed()
    }

    pub fn stdout_lines_stream(
        &self,
    ) -> futures::stream::BoxStream<'static, std::io::Result<String>> {
        self.stdout_chunked_stream().lines()
    }

    pub fn stderr_chunked_stream(
        &self,
    ) -> futures::stream::BoxStream<'static, Result<String, std::io::Error>> {
        self.history_plus_stream()
            .take_while(|res| future::ready(!matches!(res, Ok(LogMsg::Finished))))
            .filter_map(|res| async move {
                match res {
                    Ok(LogMsg::Stderr(s)) => Some(Ok(s)),
                    _ => None,
                }
            })
            .boxed()
    }

    /// Forward a stream of typed log messages into this store.
    pub fn spawn_forwarder<S, E>(self: Arc<Self>, stream: S) -> JoinHandle<()>
    where
        S: futures::Stream<Item = Result<LogMsg, E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        tokio::spawn(async move {
            tokio::pin!(stream);

            while let Some(next) = stream.next().await {
                match next {
                    Ok(msg) => self.push(msg),
                    Err(e) => self.push(LogMsg::Stderr(format!("stream error: {e}"))),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use serde_json::{Value, from_value, json};

    use super::*;

    fn entry_patch(op: &str, index: usize, timestamp: Value) -> json_patch::Patch {
        from_value(json!([{
            "op": op,
            "path": format!("/entries/{index}"),
            "value": {
                "type": "NORMALIZED_ENTRY",
                "content": {
                    "timestamp": timestamp,
                    "entry_type": { "type": "assistant_message" },
                    "content": "hello",
                },
            },
        }]))
        .unwrap()
    }

    /// Pull the `timestamp` field out of the single normalized-entry patch in history.
    fn stamped_timestamps(store: &MsgStore) -> Vec<Option<String>> {
        store
            .get_history()
            .into_iter()
            .filter_map(|msg| match msg {
                LogMsg::JsonPatch(patch) => Some(patch),
                _ => None,
            })
            .map(|patch| {
                let value = serde_json::to_value(&patch).unwrap();
                value[0]["value"]["content"]["timestamp"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    #[test]
    fn stamps_new_entries_with_wall_clock_time_when_live() {
        let store = MsgStore::new();
        let before = Utc::now();

        store.push_patch(entry_patch("add", 0, Value::Null));

        let stamped = stamped_timestamps(&store);
        let ts = stamped[0]
            .as_ref()
            .expect("live entry should be stamped")
            .parse::<DateTime<Utc>>()
            .expect("stamp should be RFC 3339");
        assert!(ts >= before && ts <= Utc::now());
    }

    #[test]
    fn reuses_first_seen_time_when_an_entry_is_replaced() {
        let store = MsgStore::new();

        store.push_patch(entry_patch("add", 7, Value::Null));
        store.push_patch(entry_patch("replace", 7, Value::Null));

        let stamped = stamped_timestamps(&store);
        assert_eq!(
            stamped[0], stamped[1],
            "a replace must keep the time the entry first appeared"
        );
    }

    #[test]
    fn replays_recorded_times_instead_of_now() {
        let recorded = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let store = MsgStore::new();
        store.set_entry_timestamps(HashMap::from([(4, recorded)]));

        store.push_patch(entry_patch("add", 4, Value::Null));

        assert_eq!(
            stamped_timestamps(&store)[0].as_deref(),
            Some(recorded.to_rfc3339().as_str())
        );
    }

    #[test]
    fn leaves_unrecorded_entries_unstamped_during_replay() {
        let store = MsgStore::new();
        store.set_entry_timestamps(HashMap::new());

        store.push_patch(entry_patch("add", 0, Value::Null));

        assert_eq!(
            stamped_timestamps(&store)[0],
            None,
            "replay must not invent a time for an entry the log never recorded"
        );
    }

    #[test]
    fn preserves_a_timestamp_the_executor_already_supplied() {
        let store = MsgStore::new();

        store.push_patch(entry_patch("add", 0, json!("2020-05-06T07:08:09+00:00")));

        assert_eq!(
            stamped_timestamps(&store)[0].as_deref(),
            Some("2020-05-06T07:08:09+00:00")
        );
    }

    #[test]
    fn leaves_non_entry_patches_untouched() {
        let store = MsgStore::new();
        let patch: json_patch::Patch = from_value(json!([{
            "op": "add",
            "path": "/entries/0",
            "value": { "type": "STDOUT", "content": "raw output" },
        }]))
        .unwrap();

        store.push_patch(patch.clone());

        match &store.get_history()[0] {
            LogMsg::JsonPatch(stored) => assert_eq!(
                serde_json::to_value(stored).unwrap(),
                serde_json::to_value(&patch).unwrap()
            ),
            other => panic!("expected a patch, got {other:?}"),
        }
    }
}
