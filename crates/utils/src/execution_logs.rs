use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::{assets::asset_dir, log_msg::LogMsg};

pub const EXECUTION_LOGS_DIRNAME: &str = "sessions";

pub fn process_logs_session_dir(session_id: Uuid) -> PathBuf {
    resolve_process_logs_session_dir(&asset_dir(), session_id)
}

pub fn process_log_file_path(session_id: Uuid, process_id: Uuid) -> PathBuf {
    process_log_file_path_in_root(&asset_dir(), session_id, process_id)
}

pub fn process_log_file_path_in_root(root: &Path, session_id: Uuid, process_id: Uuid) -> PathBuf {
    resolve_process_logs_session_dir(root, session_id)
        .join("processes")
        .join(format!("{}.jsonl", process_id))
}

pub struct ExecutionLogWriter {
    path: PathBuf,
    file: tokio::fs::File,
}

impl ExecutionLogWriter {
    pub async fn new(path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self { path, file })
    }

    pub async fn new_for_execution(session_id: Uuid, execution_id: Uuid) -> std::io::Result<Self> {
        Self::new(process_log_file_path(session_id, execution_id)).await
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn append_jsonl_line(&mut self, jsonl_line: &str) -> std::io::Result<()> {
        self.file.write_all(jsonl_line.as_bytes()).await
    }
}

pub async fn read_execution_log_file(path: &Path) -> std::io::Result<String> {
    tokio::fs::read_to_string(path).await
}

pub fn parse_log_jsonl_lossy(execution_id: Uuid, jsonl: &str) -> Vec<LogMsg> {
    let mut messages = Vec::new();
    let mut bad_lines = 0usize;

    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<LogMsg>(line) {
            Ok(msg) => messages.push(msg),
            Err(e) => {
                bad_lines += 1;
                if bad_lines <= 3 {
                    tracing::warn!(
                        "Skipping unparsable log line for execution {}: {}",
                        execution_id,
                        e
                    );
                }
            }
        }
    }

    if bad_lines > 3 {
        tracing::warn!(
            "Skipped {} unparsable log lines for execution {}",
            bad_lines,
            execution_id
        );
    }

    messages
}

/// The conversation entry index and stamped time a patch carries, if any.
///
/// `MsgStore` stamps entries on the way through, so a live patch already holds
/// the time; this reads it back out so it can be written to the log.
pub fn stamped_entry_from_patch(patch: &json_patch::Patch) -> Option<(usize, DateTime<Utc>)> {
    let value = serde_json::to_value(patch).ok()?;
    value.as_array()?.iter().find_map(|op| {
        let index = crate::msg_store::parse_entry_index(op.get("path")?.as_str()?)?;
        let entry = op.get("value")?;
        (entry.get("type")?.as_str()? == "NORMALIZED_ENTRY").then_some(())?;
        let ts = entry.get("content")?.get("timestamp")?.as_str()?;
        Some((
            index,
            DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Utc),
        ))
    })
}

/// Collect the entry times recorded in a stored log, for replaying a
/// conversation with the times it originally happened.
pub fn entry_timestamps_from_logs(messages: &[LogMsg]) -> HashMap<usize, DateTime<Utc>> {
    messages
        .iter()
        .filter_map(|msg| match msg {
            LogMsg::EntryTimestamp { index, ts } => Some((*index, *ts)),
            _ => None,
        })
        .collect()
}

fn uuid_prefix2(id: Uuid) -> String {
    let s = id.to_string();
    s.chars().take(2).collect()
}

fn resolve_process_logs_session_dir(root: &Path, session_id: Uuid) -> PathBuf {
    root.join(EXECUTION_LOGS_DIRNAME)
        .join(uuid_prefix2(session_id))
        .join(session_id.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn reads_back_entry_times_recorded_in_a_log() {
        let jsonl = concat!(
            r#"{"Stdout":"some raw agent output"}"#,
            "\n",
            r#"{"EntryTimestamp":{"index":0,"ts":"2026-01-02T03:04:05Z"}}"#,
            "\n",
            r#"{"EntryTimestamp":{"index":1,"ts":"2026-01-02T03:04:09Z"}}"#,
            "\n",
        );

        let messages = parse_log_jsonl_lossy(Uuid::nil(), jsonl);

        assert_eq!(
            entry_timestamps_from_logs(&messages),
            HashMap::from([
                (0, Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
                (1, Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 9).unwrap()),
            ])
        );
    }

    #[test]
    fn still_reads_logs_written_before_timestamps_existed() {
        let jsonl = concat!(
            r#"{"Stdout":"first"}"#,
            "\n",
            r#"{"Stderr":"oops"}"#,
            "\n",
            r#"{"Finished":null}"#,
            "\n",
        );

        let messages = parse_log_jsonl_lossy(Uuid::nil(), jsonl);

        assert!(matches!(messages[0], LogMsg::Stdout(_)));
        assert!(matches!(messages[1], LogMsg::Stderr(_)));
        assert!(
            entry_timestamps_from_logs(&messages).is_empty(),
            "an old log records no times, so entries must stay unstamped"
        );
    }

    fn stamped_patch(timestamp: serde_json::Value) -> json_patch::Patch {
        serde_json::from_value(serde_json::json!([{
            "op": "add",
            "path": "/entries/5",
            "value": {
                "type": "NORMALIZED_ENTRY",
                "content": {
                    "timestamp": timestamp,
                    "entry_type": { "type": "assistant_message" },
                    "content": "hi",
                },
            },
        }]))
        .unwrap()
    }

    #[test]
    fn finds_the_entry_time_a_patch_carries() {
        let patch = stamped_patch(serde_json::json!("2026-01-02T03:04:05+00:00"));

        assert_eq!(
            stamped_entry_from_patch(&patch),
            Some((5, Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()))
        );
    }

    #[test]
    fn records_nothing_for_an_entry_that_was_never_stamped() {
        let patch = stamped_patch(serde_json::Value::Null);

        assert_eq!(
            stamped_entry_from_patch(&patch),
            None,
            "replaying an old log must not write new time records"
        );
    }

    #[test]
    fn records_nothing_for_patches_that_carry_no_entry() {
        let patch: json_patch::Patch = serde_json::from_value(serde_json::json!([{
            "op": "add",
            "path": "/entries/0",
            "value": { "type": "STDOUT", "content": "raw" },
        }]))
        .unwrap();

        assert_eq!(stamped_entry_from_patch(&patch), None);
    }

    /// The whole point of the feature: a conversation re-read later must show
    /// the time it happened, not the time it was re-read.
    #[test]
    fn a_replayed_conversation_keeps_the_time_it_originally_ran() {
        let unstamped = stamped_patch(serde_json::Value::Null);

        // Live run: the store stamps the entry, and the writer records it.
        let live = crate::msg_store::MsgStore::new();
        live.push_patch(unstamped.clone());
        let live_patch = match &live.get_history()[0] {
            LogMsg::JsonPatch(p) => p.clone(),
            other => panic!("expected a patch, got {other:?}"),
        };
        let (index, ts) = stamped_entry_from_patch(&live_patch).expect("live run should stamp");
        let log_line = format!(
            "{}\n",
            serde_json::to_string(&LogMsg::EntryTimestamp { index, ts }).unwrap()
        );

        // Replay: same raw output, re-normalized, reading the recorded times.
        let replayed = crate::msg_store::MsgStore::new();
        replayed.set_entry_timestamps(entry_timestamps_from_logs(&parse_log_jsonl_lossy(
            Uuid::nil(),
            &log_line,
        )));
        replayed.push_patch(unstamped);
        let replayed_patch = match &replayed.get_history()[0] {
            LogMsg::JsonPatch(p) => p.clone(),
            other => panic!("expected a patch, got {other:?}"),
        };

        assert_eq!(
            stamped_entry_from_patch(&replayed_patch),
            Some((index, ts)),
            "replay must reproduce the original entry time exactly"
        );
    }

    #[test]
    fn entry_timestamp_survives_a_write_read_round_trip() {
        let original = LogMsg::EntryTimestamp {
            index: 42,
            ts: Utc.with_ymd_and_hms(2026, 8, 17, 12, 30, 0).unwrap(),
        };
        let line = format!("{}\n", serde_json::to_string(&original).unwrap());

        let messages = parse_log_jsonl_lossy(Uuid::nil(), &line);

        assert_eq!(
            entry_timestamps_from_logs(&messages),
            HashMap::from([(42, Utc.with_ymd_and_hms(2026, 8, 17, 12, 30, 0).unwrap())])
        );
    }
}
