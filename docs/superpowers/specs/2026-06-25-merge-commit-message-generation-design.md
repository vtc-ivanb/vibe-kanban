# Agent-generated merge commit messages

**Date:** 2026-06-25
**Status:** Approved design, pending implementation plan

## Problem

When a completed workspace is merged, the commit message is built synchronously
and deterministically in `crates/server/src/routes/workspaces/git.rs`:

```rust
let workspace_label = workspace.name.as_deref().unwrap_or(&workspace.branch);
let commit_message = format!("{} (vibe-kanban {})", workspace_label, vk_id);
```

This yields a one-line message derived from the workspace name/branch plus a
`(vibe-kanban <id>)` tag. There is no body and no agent involvement. The user
wants a richer, well-formed, agent-generated message (title + body), optionally
driven by a custom prompt.

The system already has a proven pattern for agent-generated text in
`trigger_pr_description_follow_up` (`crates/server/src/routes/workspaces/pr.rs`),
including a config-driven prompt (`pr_auto_description_prompt`) with a default
constant and a settings UI block. This feature mirrors that pattern.

## Decisions (from brainstorming)

- **Fully automatic**: clicking Merge generates the message, then merges with it.
  No review/edit step.
- **Fallback + warn**: if generation fails, times out, is killed, or returns an
  empty message, the merge proceeds with the current default message and the UI
  surfaces a warning.
- **Customizable prompt**: ship a default prompt; settings UI exposes a "use
  custom prompt" checkbox + textarea. Placeholders substituted server-side:
  `{task_title}`, `{task_description}`, `{vk_id}`, `{branch}`.
- **Message format**: title + body (classic git convention). No auto-appended
  `(vibe-kanban <id>)` tag; `{vk_id}` is available as a placeholder so the
  default prompt can include it if desired.
- **Async execution**: generation runs as a background agent execution; the
  merge is performed on the agent's completion. Chosen because the execution
  system is already fully async/event-driven and this avoids HTTP-timeout risk.
- **Master toggle**: `merge_commit_message_enabled`. When off, merge behaves
  exactly as today (instant, default message, no agent run).

## Architecture

### Existing infrastructure this builds on

- Agent executions are spawned as background tokio tasks and tracked in the
  `execution_processes` table (status: `Running` / `Completed` / `Failed` /
  `Killed`). See `crates/services/src/services/container.rs` and
  `crates/local-deployment/src/container.rs`.
- An **exit monitor** background task (`spawn_exit_monitor` in
  `crates/local-deployment/src/container.rs`) observes process completion,
  updates the DB, and runs follow-up actions (commit changes, next action,
  finalize, queued follow-ups).
- The frontend live-subscribes to execution-process changes over a WebSocket
  JSON-patch stream (`useExecutionProcesses.ts`,
  `crates/server/src/routes/execution_processes.rs`,
  `crates/services/src/services/events/streams.rs`). DB row changes are
  broadcast automatically via SQLite update hooks
  (`crates/services/src/services/events.rs`).

### Flow

1. **Merge request** — `POST /api/workspaces/:id/git/merge`.
   - If `merge_commit_message_enabled` is **off**: behave exactly as today
     (synchronous merge with the default message).
   - If **on**:
     - ensure the workspace container exists,
     - validate as today (no open PR, target branch not remote),
     - build the prompt (default or custom) with placeholders substituted,
     - allocate a temp-file path **outside the worktree** for the agent to
       write the message to,
     - start a **background agent execution** using the workspace's configured
       coding-agent profile, with a new run reason
       `ExecutionProcessRunReason::MergeCommitMessage`,
     - **persist a pending-merge intent** (repo_id, base branch, source branch,
       temp-file path) associated with that execution,
     - return immediately (HTTP 200 indicating generation started).

2. **Agent run** — the agent reads the branch diff (e.g. `git diff base...branch`)
   and writes **only** the commit message to the temp file. The agent makes no
   code changes. The prompt explicitly instructs: write the final commit message
   (title + body) and nothing else to `<temp path>`.

3. **Completion handler** — in the exit monitor, when a `MergeCommitMessage`
   execution completes, take a **dedicated branch** that does NOT run the normal
   commit/next-action path (the agent produced no code changes):
   - read the temp file; if non-empty use it as the commit message, otherwise
     fall back to the current default message and flag a warning,
   - perform the squash merge via `git().merge_changes()`,
   - run the existing post-merge tail: record `Merge`, trigger remote sync,
     archive the workspace (unless pinned),
   - delete the temp file.

4. **Frontend progress** — the merge button enters a "Generating commit
   message…" state while the `mergecommitmessage` execution is `running`
   (derived from the existing execution-process stream). Merge completion is
   observed via the workspace archiving, exactly as today.

### Fallback + warning delivery (async)

Because there is no blocking HTTP response to carry a warning, the warning is
surfaced through state the frontend already observes: a `mergecommitmessage`
execution that ended `Failed`/`Killed` (or produced an empty message) while the
merge still completed → the frontend shows a toast such as "Merged with default
message (generation failed)". Exact delivery mechanism (inferred-from-status vs.
a dedicated lightweight notification) is finalized in the implementation plan.

### Config

In `crates/services/src/services/config/`:

- Add `merge_commit_message_enabled: bool` (default off) — master toggle.
- Add `merge_commit_prompt: Option<String>` — custom prompt override.
- Add `DEFAULT_MERGE_COMMIT_PROMPT` constant — a good built-in prompt producing
  a title + body, referencing the available placeholders.
- A config version bump/migration adds the new fields with defaults, mirroring
  how `pr_auto_description_prompt` was introduced.

Supported placeholders (substituted server-side before dispatch):
`{task_title}`, `{task_description}`, `{vk_id}`, `{branch}`.

### Frontend settings

In `packages/web-core/.../settings/GeneralSettingsSection.tsx`, add a "Merge
commit message" block mirroring the existing PR-description block:

- enable toggle (`merge_commit_message_enabled`),
- "use custom prompt" checkbox + editable textarea (`merge_commit_prompt`),
- helper text listing the available placeholders.

## Touch-points

- `crates/db/src/models/execution_process.rs` — new
  `ExecutionProcessRunReason::MergeCommitMessage` variant.
- `crates/server/src/routes/workspaces/git.rs` — merge endpoint branches on the
  toggle; on-path it builds the prompt, allocates the temp path, persists the
  pending-merge intent, and starts the background execution.
- `crates/local-deployment/src/container.rs` — completion handler in the exit
  monitor performs the merge for `MergeCommitMessage` executions.
- `crates/services/src/services/config/` — new fields, default prompt constant,
  version migration.
- `packages/web-core` — `useMerge` (point at the generation flow / handle the
  started-vs-completed distinction), execution-status UI for the generating
  state + fallback toast, and the settings block.
- Regenerate shared TS types (`pnpm run generate-types`) after the Rust type
  changes.

## Open decision for the plan

**Where to persist the pending-merge intent.** It must outlive the HTTP request
so the exit-monitor completion handler can perform the merge. Two options:

- A small JSON metadata field on the execution-process row — survives a
  mid-flight server restart. **Recommended.**
- An in-memory registry in the container service — simpler, but dropped on
  restart (the merge would silently not happen; user retries).

Lean toward persisting on the row so a restart doesn't silently drop a merge.

## Testing

- Unit: prompt placeholder substitution.
- Unit: fallback selection — empty/missing temp file, and `Failed`/`Killed`
  execution both select the default message.
- Unit: config defaults and migration add the new fields correctly.
- Behavioral: completion handler uses the generated message when present and the
  default when absent; post-merge tail (record `Merge`, sync, archive) still
  runs in both cases.
- Toggle off: merge path is unchanged from today (no agent execution started).

## Out of scope (YAGNI)

- Draft + user-confirm / editable-before-merge UI (explicitly deferred; flow is
  fully automatic).
- Per-repo or per-task prompt overrides (single global prompt for v1).
- Regenerate/retry-from-UI button for a failed generation (user can re-merge).
