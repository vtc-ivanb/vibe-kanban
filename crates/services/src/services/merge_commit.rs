use std::path::PathBuf;

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
}
