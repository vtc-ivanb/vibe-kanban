use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{self, BufRead, Result, Write},
    path::PathBuf,
    str::FromStr,
};

use crate::executors::acp::AcpEvent;

const RESUME_HISTORY_MAX_BYTES: usize = 256 * 1024;
const RESUME_ENTRY_MAX_BYTES: usize = 16 * 1024;

/// Manages session persistence and state for ACP interactions
pub struct SessionManager {
    base_dir: PathBuf,
}

impl SessionManager {
    /// Create a new session manager with the given namespace
    pub fn new(namespace: impl Into<String>) -> Result<Self> {
        let namespace = namespace.into();
        let mut vk_dir = dirs::home_dir()
            .ok_or_else(|| io::Error::other("Could not determine home directory"))?
            .join(".vibe-kanban");

        if cfg!(debug_assertions) {
            vk_dir = vk_dir.join("dev");
        }

        let base_dir = vk_dir.join(&namespace);

        fs::create_dir_all(&base_dir)?;

        Ok(Self { base_dir })
    }

    #[cfg(test)]
    pub(crate) fn with_base_dir(base_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&base_dir)?;
        Ok(Self { base_dir })
    }

    /// Get the file path for a session
    fn session_file_path(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(format!("{session_id}.jsonl"))
    }

    fn native_session_file_path(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(format!("{session_id}.native"))
    }

    fn native_head_file_path(&self, native_id: &str) -> PathBuf {
        self.base_dir.join(format!("{native_id}.head"))
    }

    pub fn native_session_id(&self, session_id: &str) -> Result<String> {
        let path = self.native_session_file_path(session_id);
        if !path.exists() {
            return Ok(session_id.to_string());
        }

        let native_id = fs::read_to_string(path)?;
        let native_id = native_id.trim();
        if native_id.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ACP native session mapping is empty",
            ));
        }
        Ok(native_id.to_string())
    }

    pub fn native_session_is_current(&self, session_id: &str, native_id: &str) -> Result<bool> {
        let path = self.native_head_file_path(native_id);
        if !path.exists() {
            return Ok(session_id == native_id);
        }

        let current_id = fs::read_to_string(path)?;
        let current_id = current_id.trim();
        if current_id.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ACP native session head is empty",
            ));
        }
        Ok(current_id == session_id)
    }

    pub fn set_native_session_id(&self, session_id: &str, native_id: &str) -> Result<()> {
        fs::write(self.native_session_file_path(session_id), native_id)?;
        fs::write(self.native_head_file_path(native_id), session_id)
    }

    /// Append a raw JSON line to the session log
    ///
    /// We normalize ACP payloads by:
    /// - Removing top-level `sessionId`
    /// - Unwrapping the `update` envelope (store its object directly)
    /// - Dropping top-level `options` (permission menu). Note: `options` is
    ///   mutually exclusive with `update`, so when `update` is present we do not
    ///   perform any `options` stripping.
    pub fn append_raw_line(&self, session_id: &str, raw_json: &str) -> Result<()> {
        let Some(normalized) = Self::normalize_session_event(raw_json) else {
            return Ok(());
        };

        let path = self.session_file_path(session_id);
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;

        writeln!(file, "{normalized}")?;
        Ok(())
    }

    /// Attempt to normalize a raw ACP JSON event into a cleaner shape.
    /// Rules:
    /// - Remove top-level `sessionId` always.
    /// - If `update` is present with an object that has `sessionUpdate`, emit
    ///   a single-key object where key = camelCase(sessionUpdate) and value =
    ///   the `update` object minus `sessionUpdate`.
    /// - If `update` is absent, remove only top-level `options`.
    ///
    /// Returns None if the input is not a JSON object.
    fn normalize_session_event(raw_json: &str) -> Option<String> {
        let mut event = AcpEvent::from_str(raw_json).ok()?;

        match event {
            AcpEvent::SessionStart(..)
            | AcpEvent::Error(..)
            | AcpEvent::Done(..)
            | AcpEvent::Other(..) => return None,

            AcpEvent::User(..)
            | AcpEvent::Message(..)
            | AcpEvent::Thought(..)
            | AcpEvent::ToolCall(..)
            | AcpEvent::ToolUpdate(..)
            | AcpEvent::Plan(..)
            | AcpEvent::AvailableCommands(..)
            | AcpEvent::ApprovalRequested { .. }
            | AcpEvent::ApprovalResponse(..)
            | AcpEvent::CurrentMode(..) => {}

            AcpEvent::RequestPermission(req) => event = AcpEvent::ToolUpdate(req.tool_call),
        }

        match event {
            AcpEvent::User(prompt) => {
                return serde_json::to_string(&serde_json::json!({"user": prompt})).ok();
            }
            AcpEvent::Message(ref content) | AcpEvent::Thought(ref content) => {
                if let agent_client_protocol::ContentBlock::Text(text) = content {
                    // Special simplification for pure text messages
                    let key = if let AcpEvent::Message(_) = event {
                        "assistant"
                    } else {
                        "thinking"
                    };
                    return serde_json::to_string(&serde_json::json!({ key: text.text })).ok();
                }
            }
            _ => {}
        }

        serde_json::to_string(&event).ok()
    }

    /// Read the raw JSONL content of a session
    pub fn read_session_raw(&self, session_id: &str) -> Result<String> {
        let path = self.session_file_path(session_id);
        if !path.exists() {
            return Ok(String::new());
        }

        fs::read_to_string(path)
    }

    /// Fork a session to create a new one with the same history
    pub fn fork_session(&self, old_id: &str, new_id: &str) -> Result<()> {
        let old_path = self.session_file_path(old_id);
        let new_path = self.session_file_path(new_id);

        if old_path.exists() {
            fs::copy(&old_path, &new_path)?;
        } else {
            // Create empty new file if old doesn't exist
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&new_path)?;
        }

        Ok(())
    }

    /// Read a bounded tail of the session history without loading the full file.
    pub fn read_resume_history(&self, session_id: &str) -> Result<String> {
        let path = self.session_file_path(session_id);
        if !path.exists() {
            return Ok(String::new());
        }

        let mut entries = VecDeque::new();
        let mut total_bytes = 0;

        for line in io::BufReader::new(fs::File::open(path)?).lines() {
            let line = line?;
            let entry_bytes = line.len() + 1;
            if entry_bytes > RESUME_ENTRY_MAX_BYTES
                || serde_json::from_str::<serde_json::Value>(&line).is_err()
            {
                continue;
            }

            entries.push_back(line);
            total_bytes += entry_bytes;
            while total_bytes > RESUME_HISTORY_MAX_BYTES {
                let removed = entries.pop_front().expect("history is never empty");
                total_bytes -= removed.len() + 1;
            }
        }

        let mut history = entries.into_iter().collect::<Vec<_>>().join("\n");
        if !history.is_empty() {
            history.push('\n');
        }
        Ok(history)
    }

    /// Generate a resume prompt from session history
    pub fn generate_resume_prompt(&self, session_id: &str, current_prompt: &str) -> Result<String> {
        let session_context = self.read_resume_history(session_id)?;

        Ok(format!(
            concat!(
                "RESUME CONTEXT FOR CONTINUING TASK\n\n",
                "=== EXECUTION HISTORY ===\n",
                "The following is the conversation history from this session:\n",
                "{}\n\n",
                "=== CURRENT REQUEST ===\n",
                "{}\n\n",
                "=== INSTRUCTIONS ===\n",
                "You are continuing work on the above task. The execution history shows ",
                "the previous conversation in this session. Please continue from where ",
                "the previous execution left off, taking into account all the context provided above."
            ),
            session_context, current_prompt
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestSession {
        dir: PathBuf,
        manager: SessionManager,
    }

    impl TestSession {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "vibe-kanban-acp-session-test-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).unwrap();
            let manager = SessionManager {
                base_dir: dir.clone(),
            };
            Self { dir, manager }
        }

        fn write_history(&self, session_id: &str, history: &str) {
            fs::write(self.manager.session_file_path(session_id), history).unwrap();
        }

        fn raw_history(&self, session_id: &str) -> String {
            fs::read_to_string(self.manager.session_file_path(session_id)).unwrap()
        }
    }

    impl Drop for TestSession {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn assistant_entry(message: &str) -> String {
        serde_json::to_string(&serde_json::json!({ "assistant": message })).unwrap()
    }

    #[test]
    fn resume_history_is_bounded_and_leaves_source_unchanged() {
        let session = TestSession::new();
        let original = (0..2000)
            .map(|index| assistant_entry(&format!("message {index}: {}", "x".repeat(200))))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        session.write_history("large", &original);
        assert!(original.len() > RESUME_HISTORY_MAX_BYTES);

        let history = session.manager.read_resume_history("large").unwrap();

        assert!(history.len() <= RESUME_HISTORY_MAX_BYTES);
        assert_eq!(
            history.lines().last().unwrap(),
            assistant_entry(&format!("message 1999: {}", "x".repeat(200)))
        );
        assert!(!history.contains("message 0:"));
        assert!(
            history
                .lines()
                .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
        );
        assert_eq!(session.raw_history("large"), original);

        let prompt = session
            .manager
            .generate_resume_prompt("large", "do proper fix")
            .unwrap();
        assert!(prompt.contains("=== CURRENT REQUEST ===\ndo proper fix\n"));
        assert!(prompt.len() <= RESUME_HISTORY_MAX_BYTES + 1024);
    }

    #[test]
    fn resume_history_skips_malformed_and_oversized_entries() {
        let session = TestSession::new();
        let first = assistant_entry("first");
        let last = assistant_entry("last");
        let oversized = format!(
            "{{\"ToolUpdate\":{{\"output\":\"{}\"}}}}",
            "x".repeat(RESUME_ENTRY_MAX_BYTES)
        );
        session.write_history(
            "mixed",
            &format!("{first}\nnot json\n{oversized}\n{last}\n"),
        );

        let history = session.manager.read_resume_history("mixed").unwrap();

        assert_eq!(history, format!("{first}\n{last}\n"));
    }

    #[test]
    fn missing_session_has_empty_resume_history() {
        let session = TestSession::new();

        assert_eq!(session.manager.read_resume_history("missing").unwrap(), "");
    }
}
