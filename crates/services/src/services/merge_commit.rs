use std::path::PathBuf;

use executors::profile::ExecutorProfileId;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct PendingMerge {
    pub repo_id: Uuid,
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub source_branch: String,
    pub target_branch: String,
    pub message_file: PathBuf,
    pub fallback_message: String,
}

pub struct MergePromptFields<'a> {
    pub task_title: &'a str,
    pub task_description: &'a str,
    pub branch: &'a str,
    pub target_branch: &'a str,
    pub vk_id: &'a str,
    pub message_file: &'a str,
}

pub fn build_merge_commit_prompt(template: &str, fields: &MergePromptFields) -> String {
    template
        .replace("{task_title}", fields.task_title)
        .replace("{task_description}", fields.task_description)
        .replace("{branch}", fields.branch)
        .replace("{target_branch}", fields.target_branch)
        .replace("{vk_id}", fields.vk_id)
        .replace("{message_file}", fields.message_file)
}

/// Returns the generated message when it is present and non-blank, otherwise the fallback.
pub fn select_merge_commit_message(generated: Option<String>, fallback: &str) -> String {
    match generated {
        Some(msg) if !msg.trim().is_empty() => msg.trim().to_string(),
        _ => fallback.to_string(),
    }
}

/// Whether the merge-commit-message generation can resume the workspace's
/// existing agent session instead of cold-starting a new one.
///
/// The generation always runs as the configured default coding agent. Resuming
/// is only valid when the workspace's most recent coding agent uses the same
/// executor as `default`; a different executor's session format is incompatible
/// (e.g. you cannot resume a Codex session with Claude). The variant/model is
/// ignored — it only selects a model, which resume handles fine.
pub fn can_resume_session(latest: Option<&ExecutorProfileId>, default: &ExecutorProfileId) -> bool {
    latest.map(|l| &l.executor) == Some(&default.executor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_all_placeholders() {
        let out = build_merge_commit_prompt(
            "{task_title}|{task_description}|{branch}|{target_branch}|{vk_id}|{message_file}",
            &MergePromptFields {
                task_title: "T",
                task_description: "D",
                branch: "feat",
                target_branch: "main",
                vk_id: "VK-1",
                message_file: "/tmp/m.txt",
            },
        );
        assert_eq!(out, "T|D|feat|main|VK-1|/tmp/m.txt");
    }

    #[test]
    fn selects_generated_when_present() {
        assert_eq!(
            select_merge_commit_message(Some("  hello \n".to_string()), "fb"),
            "hello"
        );
    }

    #[test]
    fn falls_back_when_empty_or_missing() {
        assert_eq!(
            select_merge_commit_message(Some("   ".to_string()), "fb"),
            "fb"
        );
        assert_eq!(select_merge_commit_message(None, "fb"), "fb");
    }

    #[test]
    fn resume_only_when_executor_matches() {
        use executors::executors::BaseCodingAgent;

        let default = ExecutorProfileId::new(BaseCodingAgent::ClaudeCode);

        // Same executor, different variant → resumable (variant only picks a model).
        let same_executor =
            ExecutorProfileId::with_variant(BaseCodingAgent::ClaudeCode, "PLAN".to_string());
        assert!(can_resume_session(Some(&same_executor), &default));

        // Different executor → not resumable.
        let other = ExecutorProfileId::new(BaseCodingAgent::Codex);
        assert!(!can_resume_session(Some(&other), &default));

        // No prior agent in the workspace → not resumable.
        assert!(!can_resume_session(None, &default));
    }
}
