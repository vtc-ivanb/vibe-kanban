# Agent-generated merge commit messages — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a workspace is merged and the feature is enabled, an agent generates a well-formed (title + body) commit message before the squash merge runs; on failure the merge falls back to the current default message and warns.

**Architecture:** The merge endpoint, when enabled, starts a background coding-agent execution (new run reason `MergeCommitMessage`) that writes the message to a file in the worktree. A pending-merge intent is registered in-memory keyed by the execution-process id. When that execution completes, the exit monitor reads the file (or falls back) and performs the squash merge + the existing post-merge tail (record `Merge`, remote sync, archive). The frontend observes progress via the existing execution-process WebSocket stream.

**Tech Stack:** Rust (axum, sqlx/SQLite, tokio), ts-rs generated types, React + TypeScript (web-core), Vitest.

## Global Constraints

- Rust formatting enforced by `rustfmt`; run `pnpm run format` before completion.
- Shared TS types are generated — never hand-edit `shared/types.ts`; edit Rust + run `pnpm run generate-types`.
- SQLx offline data must be regenerated after query changes: `pnpm run prepare-db`.
- New `ExecutionProcessRunReason` serde/sql value is lowercase: `mergecommitmessage`.
- Feature is OFF by default (`merge_commit_message_enabled` defaults to `false`) — when off, merge behavior is byte-for-byte identical to today.
- Placeholders substituted server-side: `{task_title}`, `{task_description}`, `{branch}`, `{target_branch}`, `{vk_id}`, `{message_file}`.
- The generation agent must NOT commit or modify tracked files — it only writes the message file (kept untracked, excluded from the squash).
- This is a local-deployment feature; non-local `ContainerService` impls get a no-op default.

---

### Task 1: Add `MergeCommitMessage` run reason (DB + enum + exhaustive match)

**Files:**
- Create: `crates/db/migrations/20260625000000_add_merge_commit_message_run_reason.sql`
- Modify: `crates/db/src/models/execution_process.rs:53-59` (enum)
- Modify: `crates/local-deployment/src/container.rs:350` (the only exhaustive `match` on `run_reason`)

**Interfaces:**
- Produces: `ExecutionProcessRunReason::MergeCommitMessage` (serde/sql value `mergecommitmessage`).

- [ ] **Step 1: Write the migration** (mirrors `20260203000000_add_archive_script_to_repos.sql`)

```sql
-- Add 'mergecommitmessage' to the run_reason CHECK constraint.
-- SQLite cannot ALTER a CHECK constraint in place, so rebuild the column.

-- 1. Add the replacement column with the wider CHECK
ALTER TABLE execution_processes
  ADD COLUMN run_reason_new TEXT NOT NULL DEFAULT 'setupscript'
    CHECK (run_reason_new IN ('setupscript',
                               'cleanupscript',
                               'archivescript',
                               'codingagent',
                               'devserver',
                               'mergecommitmessage'));

-- 2. Copy existing values across
UPDATE execution_processes
  SET run_reason_new = run_reason;

-- 3. Drop any indexes that reference run_reason
DROP INDEX IF EXISTS idx_execution_processes_run_reason;
DROP INDEX IF EXISTS idx_execution_processes_session_status_run_reason;
DROP INDEX IF EXISTS idx_execution_processes_session_run_reason_created;

-- 4. Remove the old column
ALTER TABLE execution_processes DROP COLUMN run_reason;

-- 5. Rename the new column back to the canonical name
ALTER TABLE execution_processes
  RENAME COLUMN run_reason_new TO run_reason;

-- 6. Re-create all indexes
CREATE INDEX idx_execution_processes_run_reason
        ON execution_processes(run_reason);

CREATE INDEX idx_execution_processes_session_status_run_reason
        ON execution_processes (session_id, status, run_reason);

CREATE INDEX idx_execution_processes_session_run_reason_created
        ON execution_processes (session_id, run_reason, created_at DESC);
```

- [ ] **Step 2: Add the enum variant**

In `crates/db/src/models/execution_process.rs`, extend the enum:

```rust
pub enum ExecutionProcessRunReason {
    SetupScript,
    CleanupScript,
    ArchiveScript,
    CodingAgent,
    DevServer,
    MergeCommitMessage,
}
```

- [ ] **Step 3: Fix the one exhaustive match**

Run `grep -n "match .*run_reason" crates/local-deployment/src/container.rs` → line ~350. Read that `match` block. It currently has arms for `CodingAgent`, `CleanupScript`, and likely a catch-all or per-variant arms. Add an arm so it compiles. The block dispatches per-process setup; `MergeCommitMessage` needs no special setup here (its merge work happens at completion, Task 5), so route it through the same path as `CodingAgent` if there is no `_ =>` arm. Example (adapt to the actual arms present):

```rust
ExecutionProcessRunReason::CodingAgent
| ExecutionProcessRunReason::MergeCommitMessage => {
    // existing CodingAgent body
}
```

If the match already ends in `_ => { ... }`, no edit is needed — verify by compiling.

- [ ] **Step 4: Compile and regenerate SQLx**

Run: `cargo check -p db -p local-deployment`
Expected: compiles (no non-exhaustive-match error).
Run: `pnpm run prepare-db`
Expected: updates `.sqlx/` offline data without error.

- [ ] **Step 5: Commit**

```bash
git add crates/db/migrations/20260625000000_add_merge_commit_message_run_reason.sql \
        crates/db/src/models/execution_process.rs \
        crates/local-deployment/src/container.rs .sqlx
git commit -m "feat(db): add MergeCommitMessage execution run reason

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Config fields + default prompt

**Files:**
- Modify: `crates/services/src/services/config/mod.rs` (add `DEFAULT_MERGE_COMMIT_PROMPT` near `DEFAULT_PR_DESCRIPTION_PROMPT:10`)
- Modify: `crates/services/src/services/config/versions/v8.rs` (Config struct + default fn + both initializer sites at ~96/98 and ~152/154)
- Test: `crates/services/src/services/config/versions/v8.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `Config.merge_commit_message_enabled: bool`, `Config.merge_commit_prompt: Option<String>`, `services::services::config::DEFAULT_MERGE_COMMIT_PROMPT: &str`.

- [ ] **Step 1: Add the default prompt constant**

In `crates/services/src/services/config/mod.rs`, after the existing `DEFAULT_COMMIT_REMINDER_PROMPT` (line ~23):

```rust
pub const DEFAULT_MERGE_COMMIT_PROMPT: &str = r#"You are writing the git commit message for a squash merge of the current branch into its base branch.

Inspect the changes (e.g. run `git log {target_branch}..{branch}` and `git diff {target_branch}...{branch}`) and write a clear, well-formed commit message:
- First line: a concise summary in the imperative mood (aim for <= 72 chars).
- Then a blank line, then a body explaining WHAT changed and WHY. Use bullet points where helpful.

Context:
- Task title: {task_title}
- Task description: {task_description}
- Branch: {branch}
- Vibe Kanban ID: {vk_id}

Write ONLY the final commit message (no commentary, no markdown code fences) to this file, overwriting it if it exists:
{message_file}

Do NOT run `git commit`, do NOT stage anything, and do NOT modify any tracked files. Your only file output is the message file above."#;
```

- [ ] **Step 2: Add config fields + default fn**

In `crates/services/src/services/config/versions/v8.rs`, add a default fn near `default_commit_reminder_enabled` (line ~25):

```rust
fn default_merge_commit_message_enabled() -> bool {
    false
}
```

Add to the `Config` struct (next to `commit_reminder_prompt`):

```rust
    #[serde(default = "default_merge_commit_message_enabled")]
    pub merge_commit_message_enabled: bool,
    #[serde(default)]
    pub merge_commit_prompt: Option<String>,
```

- [ ] **Step 3: Update both initializer sites**

There are two struct literals building `Config` in `v8.rs` (the `Default`/`new` impl around line ~96 and the v7→v8 migration around line ~152). In BOTH, alongside `commit_reminder_prompt: None,` add:

```rust
            merge_commit_message_enabled: false,
            merge_commit_prompt: None,
```

- [ ] **Step 4: Write the failing test**

Append to a `#[cfg(test)] mod tests` in `v8.rs` (create the module if absent):

```rust
#[test]
fn merge_commit_message_defaults_are_off() {
    let cfg = Config::default();
    assert!(!cfg.merge_commit_message_enabled);
    assert!(cfg.merge_commit_prompt.is_none());
}
```

- [ ] **Step 5: Run the test**

Run: `cargo test -p services merge_commit_message_defaults_are_off`
Expected: PASS (after fields exist) — if it fails to compile, the field/initializer wiring is incomplete.

- [ ] **Step 6: Commit**

```bash
git add crates/services/src/services/config/
git commit -m "feat(config): add merge_commit_message settings

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Shared merge-commit helpers (prompt build + message selection + PendingMerge)

**Files:**
- Create: `crates/services/src/services/merge_commit.rs`
- Modify: `crates/services/src/services/mod.rs` (add `pub mod merge_commit;`)

**Interfaces:**
- Produces:
  - `pub struct PendingMerge { repo_id: Uuid, repo_path: PathBuf, worktree_path: PathBuf, source_branch: String, target_branch: String, message_file: PathBuf, fallback_message: String }` (all fields `pub`, derive `Clone, Debug`).
  - `pub fn build_merge_commit_prompt(template: &str, fields: &MergePromptFields) -> String`
  - `pub struct MergePromptFields<'a> { task_title: &'a str, task_description: &'a str, branch: &'a str, target_branch: &'a str, vk_id: &'a str, message_file: &'a str }`
  - `pub fn select_merge_commit_message(generated: Option<String>, fallback: &str) -> String`

- [ ] **Step 1: Write failing tests**

Create `crates/services/src/services/merge_commit.rs`:

```rust
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
        assert_eq!(select_merge_commit_message(Some("   ".to_string()), "fb"), "fb");
        assert_eq!(select_merge_commit_message(None, "fb"), "fb");
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/services/src/services/mod.rs`, add (alphabetically with the other `pub mod` lines):

```rust
pub mod merge_commit;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p services merge_commit`
Expected: 3 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/services/src/services/merge_commit.rs crates/services/src/services/mod.rs
git commit -m "feat(services): merge-commit prompt + message-selection helpers

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Pending-merge registry on the container service

**Files:**
- Modify: `crates/services/src/services/container.rs` (trait method, default no-op)
- Modify: `crates/local-deployment/src/container.rs` (struct field + impl)

**Interfaces:**
- Consumes: `PendingMerge` (Task 3).
- Produces:
  - Trait method `fn register_pending_merge(&self, execution_process_id: Uuid, pending: PendingMerge)` on `ContainerService` (default: no-op).
  - `LocalContainerService.merge_intents: Arc<RwLock<HashMap<Uuid, PendingMerge>>>` + an async `take_pending_merge(&self, id: &Uuid) -> Option<PendingMerge>`.

- [ ] **Step 1: Add the trait method with a no-op default**

In `crates/services/src/services/container.rs`, import `merge_commit::PendingMerge` and add to the `ContainerService` trait:

```rust
/// Register a pending squash-merge to be performed when the corresponding
/// `MergeCommitMessage` execution completes. Default impl is a no-op for
/// deployments that don't support agent-generated merge messages.
fn register_pending_merge(
    &self,
    _execution_process_id: uuid::Uuid,
    _pending: crate::services::merge_commit::PendingMerge,
) {
}
```

- [ ] **Step 2: Add the registry field**

In `crates/local-deployment/src/container.rs`, add to `struct LocalContainerService` (after `queued_message_service` or near other `Arc<RwLock<...>>` fields):

```rust
    merge_intents: Arc<RwLock<HashMap<Uuid, services::services::merge_commit::PendingMerge>>>,
```

Initialize it wherever `LocalContainerService` is constructed (search `LocalContainerService {` literal in the constructor / `new`): add `merge_intents: Arc::new(RwLock::new(HashMap::new())),`.

- [ ] **Step 3: Implement register + take**

In `crates/local-deployment/src/container.rs`, override the trait method inside `impl ContainerService for LocalContainerService`:

```rust
fn register_pending_merge(
    &self,
    execution_process_id: Uuid,
    pending: services::services::merge_commit::PendingMerge,
) {
    let intents = self.merge_intents.clone();
    tokio::spawn(async move {
        intents.write().await.insert(execution_process_id, pending);
    });
}
```

And add an inherent helper (in an `impl LocalContainerService` block):

```rust
async fn take_pending_merge(
    &self,
    execution_process_id: &Uuid,
) -> Option<services::services::merge_commit::PendingMerge> {
    self.merge_intents.write().await.remove(execution_process_id)
}
```

- [ ] **Step 4: Compile**

Run: `cargo check -p services -p local-deployment`
Expected: compiles.

- [ ] **Step 5: Commit**

```bash
git add crates/services/src/services/container.rs crates/local-deployment/src/container.rs
git commit -m "feat(container): pending-merge registry

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: Perform the merge on generation completion (exit monitor)

**Files:**
- Modify: `crates/local-deployment/src/container.rs` (`spawn_exit_monitor` body around line 554-794; new method `complete_merge_commit_message`)

**Interfaces:**
- Consumes: `take_pending_merge` (Task 4), `select_merge_commit_message` (Task 3), `self.git.merge_changes`, `Merge::create_direct`, `self.archive_workspace`, `self.remote_client`.
- Produces: `async fn complete_merge_commit_message(&self, ctx: &ExecutionContext)`.

- [ ] **Step 1: Branch the exit monitor before the normal flow**

In `spawn_exit_monitor`, inside `if let Ok(ctx) = ExecutionProcess::load_context(...)` (line ~554), BEFORE the `update_executor_session_summary` / `success || cleanup_done` block, short-circuit the merge-commit case:

```rust
if matches!(
    ctx.execution_process.run_reason,
    ExecutionProcessRunReason::MergeCommitMessage
) {
    container.complete_merge_commit_message(&ctx).await;
} else {
    // ... existing block: update_executor_session_summary through the
    // remote-sync-after-CodingAgent section (lines ~556-793) goes here ...
}
```

Wrap the existing body (from `update_executor_session_summary` through the CodingAgent remote-sync block) in the `else`. Keep the post-block code after line 794 (`update_after_head_commits`, MsgStore cleanup) OUTSIDE the `if/else` so it still runs for both paths.

- [ ] **Step 2: Implement the completion handler**

Add to an `impl LocalContainerService` block. This reuses the exact merge tail from the current `merge_workspace` route (`git.rs:226-263`):

```rust
async fn complete_merge_commit_message(&self, ctx: &ExecutionContext) {
    let exec_id = ctx.execution_process.id;
    let Some(pending) = self.take_pending_merge(&exec_id).await else {
        tracing::warn!("No pending merge intent for execution {exec_id}; skipping merge");
        return;
    };

    // Read the agent-written message file (best-effort), then remove it.
    let generated = tokio::fs::read_to_string(&pending.message_file).await.ok();
    let _ = tokio::fs::remove_file(&pending.message_file).await;

    let succeeded = matches!(
        ctx.execution_process.status,
        db::models::execution_process::ExecutionProcessStatus::Completed
    );
    let generated = if succeeded { generated } else { None };
    if !succeeded || generated.as_deref().map(str::trim).unwrap_or("").is_empty() {
        tracing::warn!(
            "Merge-commit-message generation for {exec_id} did not produce a message; \
             falling back to the default commit message"
        );
    }

    let commit_message = services::services::merge_commit::select_merge_commit_message(
        generated,
        &pending.fallback_message,
    );

    let merge_commit_id = match self.git.merge_changes(
        &pending.repo_path,
        &pending.worktree_path,
        &pending.source_branch,
        &pending.target_branch,
        &commit_message,
    ) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Failed to merge workspace {} after message generation: {e}", ctx.workspace.id);
            return;
        }
    };

    if let Err(e) = Merge::create_direct(
        &self.db.pool,
        ctx.workspace.id,
        pending.repo_id,
        &pending.target_branch,
        &merge_commit_id,
    )
    .await
    {
        tracing::error!("Failed to record merge for workspace {}: {e}", ctx.workspace.id);
    }

    if let Some(client) = &self.remote_client {
        let client = client.clone();
        let workspace_id = ctx.workspace.id;
        tokio::spawn(async move {
            remote_sync::sync_local_workspace_merge_to_remote(&client, workspace_id).await;
        });
    }

    if !ctx.workspace.pinned
        && let Err(e) = self.archive_workspace(ctx.workspace.id).await
    {
        tracing::error!("Failed to archive workspace {}: {e}", ctx.workspace.id);
    }
}
```

Add imports as needed at the top of the file: `db::models::merge::Merge`, `services::services::remote_sync`. (`ExecutionProcessStatus`, `ExecutionContext` are already in scope in this file.)

- [ ] **Step 3: Compile**

Run: `cargo check -p local-deployment`
Expected: compiles. Resolve any missing imports.

- [ ] **Step 4: Run the backend test suite (sanity)**

Run: `cargo test -p local-deployment`
Expected: existing tests still PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/local-deployment/src/container.rs
git commit -m "feat(container): perform squash merge after message generation

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 6: Merge endpoint — start generation when enabled

**Files:**
- Modify: `crates/server/src/routes/workspaces/git.rs:178-266` (`merge_workspace`, response type)
- Modify: `crates/server/src/bin/generate_types.rs` (register the new response struct if added)

**Interfaces:**
- Consumes: `Config.merge_commit_message_enabled`, `Config.merge_commit_prompt`, `DEFAULT_MERGE_COMMIT_PROMPT`, `build_merge_commit_prompt`, `PendingMerge`, `register_pending_merge`, `Task::find_by_id`.
- Produces: `struct MergeWorkspaceResponse { generating: bool }`; `merge_workspace` returns `ApiResponse<MergeWorkspaceResponse>`.

- [ ] **Step 1: Add the response struct**

In `git.rs`, near `MergeWorkspaceRequest` (line 59):

```rust
#[derive(Debug, Serialize, Deserialize, TS)]
pub struct MergeWorkspaceResponse {
    /// True when an agent is generating the commit message and the merge will
    /// complete asynchronously; false when the merge already completed inline.
    pub generating: bool,
}
```

- [ ] **Step 2: Refactor `merge_workspace`**

Keep all validation up to and including computing `worktree_path` (lines 184-220) unchanged. Compute the fallback message exactly as today, then branch on the toggle:

```rust
    let workspace_label = workspace.name.as_deref().unwrap_or(&workspace.branch);
    let vk_id = resolve_vibe_kanban_identifier(&deployment, workspace.id).await;
    let fallback_message = format!("{} (vibe-kanban {})", workspace_label, vk_id);

    let (enabled, prompt_template) = {
        let config = deployment.config().read().await;
        (
            config.merge_commit_message_enabled,
            config
                .merge_commit_prompt
                .clone()
                .unwrap_or_else(|| services::services::config::DEFAULT_MERGE_COMMIT_PROMPT.to_string()),
        )
    };

    if !enabled {
        // Unchanged synchronous merge path.
        let merge_commit_id = deployment.git().merge_changes(
            &repo.path,
            &worktree_path,
            &workspace.branch,
            &workspace_repo.target_branch,
            &fallback_message,
        )?;
        Merge::create_direct(
            pool,
            workspace.id,
            workspace_repo.repo_id,
            &workspace_repo.target_branch,
            &merge_commit_id,
        )
        .await?;
        if let Ok(client) = deployment.remote_client() {
            let workspace_id = workspace.id;
            tokio::spawn(async move {
                remote_sync::sync_local_workspace_merge_to_remote(&client, workspace_id).await;
            });
        }
        if !workspace.pinned
            && let Err(e) = deployment.container().archive_workspace(workspace.id).await
        {
            tracing::error!("Failed to archive workspace {}: {}", workspace.id, e);
        }
        deployment
            .track_if_analytics_allowed(
                "task_attempt_merged",
                serde_json::json!({ "workspace_id": workspace.id.to_string() }),
            )
            .await;
        return Ok(ResponseJson(ApiResponse::success(MergeWorkspaceResponse {
            generating: false,
        })));
    }
```

Then implement the enabled path. Resolve task title/description, build the message-file path (worktree-local, untracked), build the prompt, start the execution, and register the pending merge:

```rust
    // Resolve task title/description for placeholders.
    let (task_title, task_description) = if let Some(task_id) = workspace.task_id {
        match db::models::task::Task::find_by_id(pool, task_id).await? {
            Some(task) => (task.title, task.description.unwrap_or_default()),
            None => (workspace_label.to_string(), String::new()),
        }
    } else {
        (workspace_label.to_string(), String::new())
    };

    // Message file lives in the worktree root, stays untracked (excluded from the
    // squash, which merges committed trees), and is removed after reading.
    let message_file = worktree_path.join(".vk-merge-commit-msg.txt");

    let prompt = services::services::merge_commit::build_merge_commit_prompt(
        &prompt_template,
        &services::services::merge_commit::MergePromptFields {
            task_title: &task_title,
            task_description: &task_description,
            branch: &workspace.branch,
            target_branch: &workspace_repo.target_branch,
            vk_id: &vk_id,
            message_file: &message_file.to_string_lossy(),
        },
    );

    // Get-or-create a session + executor profile, mirroring trigger_pr_description_follow_up.
    let session = match db::models::session::Session::find_latest_by_workspace_id(pool, workspace.id).await? {
        Some(s) => s,
        None => {
            db::models::session::Session::create(
                pool,
                &db::models::session::CreateSession { executor: None, name: None },
                Uuid::new_v4(),
                workspace.id,
            )
            .await?
        }
    };

    let Some(executor_profile_id) = ExecutionProcess::latest_executor_profile_for_session(pool, session.id).await? else {
        // No agent has run here; fall back to a synchronous default merge.
        let merge_commit_id = deployment.git().merge_changes(
            &repo.path, &worktree_path, &workspace.branch, &workspace_repo.target_branch, &fallback_message,
        )?;
        Merge::create_direct(pool, workspace.id, workspace_repo.repo_id, &workspace_repo.target_branch, &merge_commit_id).await?;
        if !workspace.pinned {
            let _ = deployment.container().archive_workspace(workspace.id).await;
        }
        return Ok(ResponseJson(ApiResponse::success(MergeWorkspaceResponse { generating: false })));
    };

    let latest_session_info =
        db::models::coding_agent_turn::CodingAgentTurn::find_latest_session_info(pool, session.id).await?;
    let working_dir = session.agent_working_dir.as_ref().filter(|d| !d.is_empty()).cloned();

    let action_type = if let Some(info) = latest_session_info {
        ExecutorActionType::CodingAgentFollowUpRequest(CodingAgentFollowUpRequest {
            prompt,
            session_id: info.session_id,
            reset_to_message_id: None,
            executor_config: executors::profile::ExecutorConfig::from(executor_profile_id.clone()),
            working_dir: working_dir.clone(),
        })
    } else {
        ExecutorActionType::CodingAgentInitialRequest(CodingAgentInitialRequest {
            prompt,
            executor_config: executors::profile::ExecutorConfig::from(executor_profile_id.clone()),
            working_dir,
        })
    };
    let action = ExecutorAction::new(action_type, None);

    let execution_process = deployment
        .container()
        .start_execution(&workspace, &session, &action, &ExecutionProcessRunReason::MergeCommitMessage)
        .await?;

    deployment.container().register_pending_merge(
        execution_process.id,
        services::services::merge_commit::PendingMerge {
            repo_id: workspace_repo.repo_id,
            repo_path: repo.path.clone(),
            worktree_path: worktree_path.clone(),
            source_branch: workspace.branch.clone(),
            target_branch: workspace_repo.target_branch.clone(),
            message_file,
            fallback_message,
        },
    );

    Ok(ResponseJson(ApiResponse::success(MergeWorkspaceResponse { generating: true })))
```

Add the needed imports to `git.rs`: `db::models::execution_process::{ExecutionProcess, ExecutionProcessRunReason}`, `executors::actions::{ExecutorAction, ExecutorActionType, coding_agent_follow_up::CodingAgentFollowUpRequest, coding_agent_initial::CodingAgentInitialRequest}`. Update the `merge_workspace` return type to `Result<ResponseJson<ApiResponse<MergeWorkspaceResponse>>, ApiError>`.

> Note on race: `register_pending_merge` runs immediately after `start_execution` returns (the agent then runs for seconds), so the intent is registered well before completion. The completion handler logs and no-ops if the intent is absent.

- [ ] **Step 3: Register the response type for TS generation**

In `crates/server/src/bin/generate_types.rs`, next to `MergeWorkspaceRequest::decl()` (line 121) add:

```rust
        server::routes::workspaces::git::MergeWorkspaceResponse::decl(),
```

- [ ] **Step 4: Compile + generate types**

Run: `cargo check -p server`
Expected: compiles.
Run: `pnpm run generate-types`
Expected: `shared/types.ts` gains `MergeWorkspaceResponse` and the `mergecommitmessage` enum value; no diff errors.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/routes/workspaces/git.rs crates/server/src/bin/generate_types.rs shared/types.ts
git commit -m "feat(server): start agent merge-message generation on merge

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 7: Frontend — merge mutation + generating/fallback UX

**Files:**
- Modify: `packages/web-core/src/shared/lib/api.ts:557-569` (`merge` return type)
- Modify: `packages/web-core/src/shared/hooks/useMerge.ts`
- Modify: the merge call site that shows success (find via `useMerge(` usages) to handle `generating`.

**Interfaces:**
- Consumes: `MergeWorkspaceResponse` from `shared/types.ts`.
- Produces: `useMerge` resolves to `MergeWorkspaceResponse`; callers branch on `.generating`.

- [ ] **Step 1: Update the API client return type**

In `api.ts`, change `merge` to return the typed response:

```ts
  merge: async (
    workspaceId: string,
    data: MergeWorkspaceRequest
  ): Promise<MergeWorkspaceResponse> => {
    const response = await makeRequest(
      `/api/workspaces/${workspaceId}/git/merge`,
      { method: 'POST', body: JSON.stringify(data) }
    );
    return handleApiResponse<MergeWorkspaceResponse>(response);
  },
```

Import `MergeWorkspaceResponse` from the shared types barrel used elsewhere in the file (match the existing `MergeWorkspaceRequest` import).

- [ ] **Step 2: Update `useMerge`**

```ts
import { MergeWorkspaceResponse } from 'shared/types'; // match existing import style

export function useMerge(
  workspaceId?: string,
  onSuccess?: (result: MergeWorkspaceResponse) => void,
  onError?: (err: unknown) => void
) {
  const queryClient = useQueryClient();

  return useMutation<MergeWorkspaceResponse, unknown, MergeParams>({
    mutationFn: (params: MergeParams) => {
      if (!workspaceId) return Promise.resolve({ generating: false });
      return workspacesApi.merge(workspaceId, { repo_id: params.repoId });
    },
    onSuccess: (result) => {
      queryClient.invalidateQueries({ queryKey: ['branchStatus', workspaceId] });
      queryClient.invalidateQueries({ queryKey: repoBranchKeys.all });
      onSuccess?.(result);
    },
    onError: (err) => {
      console.error('Failed to merge:', err);
      onError?.(err);
    },
  });
}
```

- [ ] **Step 3: Handle `generating` at the call site**

Find the call site(s): `grep -rn "useMerge(" packages/web-core/src`. In the `onSuccess` callback, branch: when `result.generating`, show a toast/inline state like "Generating commit message…" instead of treating the workspace as already merged (don't close/navigate yet). The existing execution-process stream (`useExecutionProcesses`) will show the `mergecommitmessage` process running; completion is observed via the workspace archiving (same signal as today).

- [ ] **Step 4: Fallback warning toast**

In the merge call site, after a merge that was `generating`, detect the failure-but-merged case: when the workspace's latest `mergecommitmessage` execution-process ends with status `failed`/`killed` while the workspace becomes archived, show a toast: "Merged with default commit message (generation failed)." Use the existing execution-process subscription (see `useExecutionProcesses.ts`) to read that process's status. Keep this best-effort.

- [ ] **Step 5: Type-check**

Run: `pnpm run check`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add packages/web-core/src/shared/lib/api.ts packages/web-core/src/shared/hooks/useMerge.ts <call-site files>
git commit -m "feat(web): handle async merge-message generation

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 8: Frontend settings block + finalize

**Files:**
- Modify: `packages/web-core/src/shared/dialogs/settings/settings/GeneralSettingsSection.tsx` (add a "Merge commit message" `SettingsCard` after the Commits card)
- Modify: the TS constants module exporting `DEFAULT_PR_DESCRIPTION_PROMPT` (find via its import in `GeneralSettingsSection.tsx`) — add `DEFAULT_MERGE_COMMIT_PROMPT`.
- Modify: the en locale JSON (find via `grep -rln "autoDescription" packages/web-core/src`) — add `settings.general.mergeCommit.*` keys.

**Interfaces:**
- Consumes: `Config.merge_commit_message_enabled`, `Config.merge_commit_prompt` (now on the generated `Config` type).

- [ ] **Step 1: Add the TS default prompt constant**

In the same module that defines `DEFAULT_PR_DESCRIPTION_PROMPT`, add `DEFAULT_MERGE_COMMIT_PROMPT` with the SAME string as `DEFAULT_MERGE_COMMIT_PROMPT` in `config/mod.rs` (copy verbatim). Export it.

- [ ] **Step 2: Add the settings card**

After the `{/* Commits */}` `SettingsCard` block, add (mirrors the PR block):

```tsx
      {/* Merge commit message */}
      <SettingsCard
        title={t('settings.general.mergeCommit.title')}
        description={t('settings.general.mergeCommit.description')}
      >
        <SettingsCheckbox
          id="merge-commit-message"
          label={t('settings.general.mergeCommit.enabled.label')}
          description={t('settings.general.mergeCommit.enabled.helper')}
          checked={draft?.merge_commit_message_enabled ?? false}
          onChange={(checked) =>
            updateDraft({ merge_commit_message_enabled: checked })
          }
        />

        {draft?.merge_commit_message_enabled && (
          <>
            <SettingsCheckbox
              id="use-custom-merge-commit-prompt"
              label={t('settings.general.mergeCommit.customPrompt.useCustom')}
              checked={draft?.merge_commit_prompt != null}
              onChange={(checked) => {
                if (checked) {
                  updateDraft({ merge_commit_prompt: DEFAULT_MERGE_COMMIT_PROMPT });
                } else {
                  updateDraft({ merge_commit_prompt: null });
                }
              }}
            />
            <SettingsField
              label=""
              description={t('settings.general.mergeCommit.customPrompt.helper')}
            >
              <SettingsTextarea
                value={draft?.merge_commit_prompt ?? DEFAULT_MERGE_COMMIT_PROMPT}
                onChange={(value) => updateDraft({ merge_commit_prompt: value })}
                disabled={draft?.merge_commit_prompt == null}
              />
            </SettingsField>
          </>
        )}
      </SettingsCard>
```

Import `DEFAULT_MERGE_COMMIT_PROMPT` alongside the existing `DEFAULT_PR_DESCRIPTION_PROMPT` import.

- [ ] **Step 3: Add locale keys**

In the en locale file, add under `settings.general` (mirror `pullRequests`):

```json
"mergeCommit": {
  "title": "Merge commit message",
  "description": "Have an agent generate the commit message when merging.",
  "enabled": {
    "label": "Generate commit message with an agent",
    "helper": "When merging, an agent writes a title + body commit message. Falls back to the default message if generation fails."
  },
  "customPrompt": {
    "useCustom": "Use a custom prompt",
    "helper": "Available placeholders: {task_title}, {task_description}, {branch}, {target_branch}, {vk_id}, {message_file}."
  }
}
```

- [ ] **Step 4: Lint, type-check, format**

Run: `pnpm run check`
Expected: PASS.
Run: `pnpm run lint`
Expected: PASS.
Run: `pnpm run format`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add packages/web-core/
git commit -m "feat(web): merge commit message settings

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Final verification

- [ ] `cargo test --workspace` passes.
- [ ] `pnpm run check` and `pnpm run lint` pass.
- [ ] Manual: enable the setting, merge a completed workspace, confirm the merge commit has an agent-generated title + body; disable it and confirm the old one-line message; simulate failure (e.g. kill the agent) and confirm fallback + warning.

## Notes / deferred (YAGNI)

- Pending-merge intent is in-memory: a server restart mid-generation drops the merge (user re-merges). Persisting on the execution-process row is a future hardening if this proves flaky.
- Draft+confirm UI, per-repo prompts, and a manual "regenerate" button are out of scope (see spec).
