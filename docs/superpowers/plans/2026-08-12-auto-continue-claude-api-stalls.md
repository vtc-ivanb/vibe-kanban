# Auto-continue Claude API Stalls Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Automatically resume a Claude coding-agent turn that the `claude` CLI aborted due to a transient Anthropic API error (e.g. "Response stalled mid-stream"), instead of leaving the run failed.

**Architecture:** The Claude executor classifies terminal `is_error` results as retryable vs fatal and emits a structured `RetryableApiError` normalized entry. The container's exit monitor, on a failed coding-agent run whose history contains that entry, resumes the session with a short "continue" follow-up — bounded to 3 consecutive auto-continues and gated by a config flag.

**Tech Stack:** Rust (tokio, sqlx, serde, ts-rs), SQLite, React/TypeScript (generated types only).

## Global Constraints

- Claude executor only. Do not touch other executors (Codex, Gemini, Cursor, etc.).
- `MAX_AUTO_CONTINUE_RETRIES = 3` (at most 3 consecutive auto-continues per turn chain).
- `AUTO_CONTINUE_BACKOFF = Duration::from_secs(2)` before spawning the retry.
- Auto-continue prompt (verbatim): `Please continue from where you were interrupted.`
- Unknown / unrecognized `is_error` text classifies as **Fatal** (never retry).
- Config flag `auto_continue_on_api_stall_enabled` defaults to `true`.
- Retryable-marker string used for log scans: `retryable_api_error` (the serde tag of the new enum variant).
- Run `pnpm run generate-types` after changing any `#[derive(TS)]` type; never hand-edit `shared/types.ts`.
- Run `pnpm run format` before the final commit.

---

### Task 1: Add `RetryableApiError` to `NormalizedEntryError`

**Files:**
- Modify: `crates/executors/src/logs/mod.rs:63-68`
- Regenerate: `shared/types.ts` (via command, do not hand-edit)

**Interfaces:**
- Produces: `NormalizedEntryError::RetryableApiError` — serialized as `{"type":"retryable_api_error"}` because the enum has `#[serde(rename_all = "snake_case")]`.

- [ ] **Step 1: Add the variant**

In `crates/executors/src/logs/mod.rs`, change:

```rust
pub enum NormalizedEntryError {
    SetupRequired,
    Other,
}
```

to:

```rust
pub enum NormalizedEntryError {
    SetupRequired,
    /// A transient API-layer error the agent turn can be safely auto-resumed from
    /// (e.g. a mid-stream stall, overload, or timeout).
    RetryableApiError,
    Other,
}
```

- [ ] **Step 2: Confirm it compiles**

Run: `cargo check -p executors`
Expected: PASS.

- [ ] **Step 3: Regenerate shared types**

Run: `pnpm run generate-types`
Expected: `shared/types.ts` now contains `retryable_api_error` in the `NormalizedEntryError` union. Confirm with:
`grep -n "retryable_api_error" shared/types.ts`

- [ ] **Step 4: Commit**

```bash
git add crates/executors/src/logs/mod.rs shared/types.ts
git commit -m "feat(executors): add RetryableApiError normalized entry variant"
```

---

### Task 2: Classify terminal Claude `is_error` results

**Files:**
- Modify: `crates/executors/src/executors/claude.rs` (add free function + `#[cfg(test)]` tests near the existing tests, e.g. after line ~2320 where the test module or type defs live)

**Interfaces:**
- Produces: `fn classify_claude_api_error(result_text: Option<&str>, error_field: Option<&str>) -> ClaudeApiErrorClass` and `enum ClaudeApiErrorClass { Retryable, Fatal }`.
- Consumes: nothing from other tasks.

- [ ] **Step 1: Write the failing tests**

Add near the bottom of `crates/executors/src/executors/claude.rs`, inside the existing `#[cfg(test)] mod tests { ... }` block:

```rust
#[test]
fn classifies_stall_as_retryable() {
    let class = classify_claude_api_error(
        Some("API Error: Response stalled mid-stream. The response above may be incomplete."),
        None,
    );
    assert_eq!(class, ClaudeApiErrorClass::Retryable);
}

#[test]
fn classifies_overloaded_as_retryable() {
    let class = classify_claude_api_error(Some("API Error: 529 overloaded_error"), None);
    assert_eq!(class, ClaudeApiErrorClass::Retryable);
}

#[test]
fn classifies_auth_error_as_fatal() {
    // Real capture: a 401 mid-turn.
    let class = classify_claude_api_error(
        Some("Failed to authenticate. API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth token has expired.\"}}"),
        Some("authentication_failed"),
    );
    assert_eq!(class, ClaudeApiErrorClass::Fatal);
}

#[test]
fn classifies_prompt_too_long_as_fatal() {
    let class = classify_claude_api_error(Some("API Error: 400 prompt is too long"), None);
    assert_eq!(class, ClaudeApiErrorClass::Fatal);
}

#[test]
fn classifies_unknown_as_fatal() {
    let class = classify_claude_api_error(Some("some entirely unexpected error"), None);
    assert_eq!(class, ClaudeApiErrorClass::Fatal);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p executors classifies_ -- --nocapture`
Expected: FAIL — `classify_claude_api_error` / `ClaudeApiErrorClass` not found.

- [ ] **Step 3: Implement the classifier**

Add (module-level, not inside the test block) in `crates/executors/src/executors/claude.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeApiErrorClass {
    Retryable,
    Fatal,
}

/// Classify a terminal Claude `result` (with `is_error: true`) as a transient
/// error worth auto-resuming, or a fatal one that must not be retried.
///
/// Fatal wins on any fatal signal, and anything unrecognized is Fatal so we
/// never loop on an unknown error.
pub(crate) fn classify_claude_api_error(
    result_text: Option<&str>,
    error_field: Option<&str>,
) -> ClaudeApiErrorClass {
    let haystack = format!(
        "{} {}",
        result_text.unwrap_or_default(),
        error_field.unwrap_or_default()
    )
    .to_lowercase();

    const FATAL: &[&str] = &[
        "authentication_error",
        "authentication_failed",
        "oauth token has expired",
        " 401",
        " 403",
        "invalid_request",
        " 400",
        "prompt is too long",
        "credit balance",
        "billing",
    ];
    const RETRYABLE: &[&str] = &[
        "stalled mid-stream",
        "response stalled",
        "overloaded",
        " 529",
        " 500",
        "internal server error",
        "timed out",
        "timeout",
        "connection error",
    ];

    if FATAL.iter().any(|p| haystack.contains(p)) {
        return ClaudeApiErrorClass::Fatal;
    }
    if RETRYABLE.iter().any(|p| haystack.contains(p)) {
        return ClaudeApiErrorClass::Retryable;
    }
    ClaudeApiErrorClass::Fatal
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p executors classifies_`
Expected: PASS (all 5).

- [ ] **Step 5: Commit**

```bash
git add crates/executors/src/executors/claude.rs
git commit -m "feat(executors): classify transient Claude API errors as retryable"
```

---

### Task 3: Emit a `RetryableApiError` entry for terminal `is_error` results

**Files:**
- Modify: `crates/executors/src/executors/claude.rs:1826-1844` (the `ClaudeJson::Result` handling; currently only `AmpResume` emits an error entry)

**Interfaces:**
- Consumes: `classify_claude_api_error`, `ClaudeApiErrorClass` (Task 2); `NormalizedEntryError::RetryableApiError` (Task 1).
- Produces: a `NormalizedEntry` with `entry_type: ErrorMessage { error_type }` whenever a terminal `is_error` result arrives on any Claude strategy.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/executors/src/executors/claude.rs`:

```rust
#[test]
fn stall_result_emits_retryable_error_entry() {
    let result_json = r#"{"type":"result","subtype":"success","is_error":true,"num_turns":1,"result":"API Error: Response stalled mid-stream. The response above may be incomplete."}"#;
    let msg_store = std::sync::Arc::new(utils::msg_store::MsgStore::new());
    msg_store.push_stdout(format!("{result_json}\n"));
    msg_store.push_finished();

    let entries = normalized_entries_for_test(&msg_store); // existing helper used by other tests
    assert!(
        entries.iter().any(|e| matches!(
            &e.entry_type,
            NormalizedEntryType::ErrorMessage {
                error_type: NormalizedEntryError::RetryableApiError
            }
        )),
        "expected a RetryableApiError entry, got: {entries:?}"
    );
}
```

Note: reuse whatever helper the existing tests use to drive `normalize_logs` and collect `NormalizedEntry` values (search the test module for how `result_json` fixtures are currently turned into entries, e.g. around `claude.rs:2843-2890`). If no such helper exists, inline the same setup those tests use.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p executors stall_result_emits_retryable_error_entry`
Expected: FAIL — no `RetryableApiError` entry is produced (today a non-AmpResume `is_error` result emits no error entry).

- [ ] **Step 3: Implement the emission**

In `crates/executors/src/executors/claude.rs`, the `ClaudeJson::Result` arm currently reads (around lines 1826-1844):

```rust
if empty_bg_continuation {
    // nothing to surface
} else if matches!(self.strategy, HistoryStrategy::AmpResume)
    && is_error.unwrap_or(false)
{
    let entry = NormalizedEntry {
        timestamp: None,
        entry_type: NormalizedEntryType::ErrorMessage {
            error_type: NormalizedEntryError::Other,
        },
        content: serde_json::to_string(claude_json)
            .unwrap_or_else(|_| "error".to_string()),
        metadata: Some(
            serde_json::to_value(claude_json).unwrap_or(serde_json::Value::Null),
        ),
    };
    let idx = entry_index_provider.next();
    patches.push(ConversationPatch::add_normalized_entry(idx, entry));
} else if matches!(subtype.as_deref(), Some("success")) ...
```

Replace the `else if matches!(self.strategy, HistoryStrategy::AmpResume) && is_error.unwrap_or(false)` branch with a strategy-independent one that classifies the error:

```rust
} else if is_error.unwrap_or(false) {
    let result_text = result.as_ref().and_then(|v| v.as_str());
    let error_type = match classify_claude_api_error(result_text, error.as_deref()) {
        ClaudeApiErrorClass::Retryable => NormalizedEntryError::RetryableApiError,
        ClaudeApiErrorClass::Fatal => NormalizedEntryError::Other,
    };
    let content = result_text
        .map(|s| s.to_string())
        .unwrap_or_else(|| serde_json::to_string(claude_json).unwrap_or_else(|_| "error".to_string()));
    let entry = NormalizedEntry {
        timestamp: None,
        entry_type: NormalizedEntryType::ErrorMessage { error_type },
        content,
        metadata: Some(
            serde_json::to_value(claude_json).unwrap_or(serde_json::Value::Null),
        ),
    };
    let idx = entry_index_provider.next();
    patches.push(ConversationPatch::add_normalized_entry(idx, entry));
} else if matches!(subtype.as_deref(), Some("success")) ...
```

The `ClaudeJson::Result` arm already binds `is_error`, `subtype`, `result`, `origin`, `num_turns`, `model_usage`; add `error,` to the destructured fields at the top of the arm (around `claude.rs:1797-1805`) so `error.as_deref()` is available.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p executors stall_result_emits_retryable_error_entry`
Expected: PASS.
Run the AmpResume-related tests too: `cargo test -p executors amp` — expected still PASS (AmpResume `is_error` results now go through the same branch and still produce an ErrorMessage entry; adjust any AmpResume test that asserted the exact old `content` string to accept the result text).

- [ ] **Step 5: Commit**

```bash
git add crates/executors/src/executors/claude.rs
git commit -m "feat(executors): surface terminal Claude API errors as normalized entries"
```

---

### Task 4: Add the `auto_continue_on_api_stall_enabled` config flag

**Files:**
- Modify: `crates/services/src/services/config/versions/v8.rs:40-79` (struct + `from_v7_config`) and add a default helper.

**Interfaces:**
- Produces: `Config.auto_continue_on_api_stall_enabled: bool` (default `true`).

- [ ] **Step 1: Add the field**

In `crates/services/src/services/config/versions/v8.rs`, add to `struct Config` (after `host_nickname`):

```rust
    #[serde(default = "default_auto_continue_on_api_stall_enabled")]
    pub auto_continue_on_api_stall_enabled: bool,
```

Add the default helper near the other `default_*` fns in the same file:

```rust
fn default_auto_continue_on_api_stall_enabled() -> bool {
    true
}
```

Set it in `from_v7_config` (in the `Self { ... }` literal, around line 108):

```rust
    auto_continue_on_api_stall_enabled: true,
```

Also add it to any other `Config { ... }` literal / `Default` impl in the file that the compiler flags.

- [ ] **Step 2: Confirm it compiles and types regen cleanly**

Run: `cargo check -p services`
Expected: PASS.
Run: `pnpm run generate-types`
Expected: `auto_continue_on_api_stall_enabled` appears in `shared/types.ts` `Config`. Confirm: `grep -n "auto_continue_on_api_stall_enabled" shared/types.ts`

- [ ] **Step 3: Commit**

```bash
git add crates/services/src/services/config/versions/v8.rs shared/types.ts
git commit -m "feat(config): add auto_continue_on_api_stall_enabled flag (default on)"
```

---

### Task 5: Container helpers — detect retryable stall + count consecutive auto-continues

**Files:**
- Modify: `crates/local-deployment/src/container.rs` (add two helper methods on `LocalContainerService`, near `extract_last_assistant_message` at line 946)

**Interfaces:**
- Consumes: `NormalizedEntryType::ErrorMessage`, `NormalizedEntryError::RetryableApiError` (Tasks 1/3); `extract_normalized_entry_from_patch` (already imported, `container.rs:38`).
- Produces:
  - `fn last_turn_ended_in_retryable_stall(&self, exec_id: &Uuid) -> bool`
  - `async fn consecutive_auto_continue_count(&self, session_id: Uuid, before_exec_id: Uuid) -> u32`

- [ ] **Step 1: Write the failing test (in-memory history scan)**

Add a `#[cfg(test)]` test in `crates/local-deployment/src/container.rs`. Test only the pure history-scan predicate by extracting its inner logic into a free function `history_has_retryable_stall(history: &[LogMsg]) -> bool` and testing that:

```rust
#[test]
fn detects_retryable_stall_in_history() {
    use executors::logs::{NormalizedEntry, NormalizedEntryType, NormalizedEntryError};
    use executors::logs::utils::ConversationPatch;
    let entry = NormalizedEntry {
        timestamp: None,
        entry_type: NormalizedEntryType::ErrorMessage {
            error_type: NormalizedEntryError::RetryableApiError,
        },
        content: "API Error: Response stalled mid-stream.".to_string(),
        metadata: None,
    };
    let patch = ConversationPatch::add_normalized_entry(0, entry);
    let history = vec![LogMsg::JsonPatch(patch)];
    assert!(history_has_retryable_stall(&history));
}

#[test]
fn no_retryable_stall_in_empty_history() {
    assert!(!history_has_retryable_stall(&[]));
}
```

(Use the exact `ConversationPatch` constructor path the codebase exposes — mirror the import used at `container.rs:38` / how `extract_normalized_entry_from_patch` is applied.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p local-deployment detects_retryable_stall_in_history`
Expected: FAIL — `history_has_retryable_stall` not found.

- [ ] **Step 3: Implement the helpers**

Add to `crates/local-deployment/src/container.rs`:

```rust
/// Pure scan: does this MsgStore history contain a terminal retryable API-error entry?
fn history_has_retryable_stall(history: &[LogMsg]) -> bool {
    history.iter().rev().any(|msg| {
        if let LogMsg::JsonPatch(patch) = msg
            && let Some((_, entry)) = extract_normalized_entry_from_patch(patch)
        {
            matches!(
                entry.entry_type,
                NormalizedEntryType::ErrorMessage {
                    error_type: NormalizedEntryError::RetryableApiError
                }
            )
        } else {
            false
        }
    })
}
```

Then the method wrapper (imports: add `NormalizedEntryError` to the `logs::{...}` use at `container.rs:38`):

```rust
fn last_turn_ended_in_retryable_stall(&self, exec_id: &Uuid) -> bool {
    let Some(msg_stores) = self.msg_stores.try_read().ok() else { return false };
    let Some(msg_store) = msg_stores.get(exec_id) else { return false };
    history_has_retryable_stall(&msg_store.get_history())
}
```

And the stateless consecutive counter.

> **IMPORTANT — counter data source (design correction).** The normalized
> `retryable_api_error` entry is delivered as a `LogMsg::JsonPatch`, which is
> **never persisted**: `execution_process_logs` is emptied at startup by
> `migrate_execution_logs_to_files`, and the raw-log persister
> (`spawn_stream_raw_logs_to_storage`, `crates/services/src/services/execution_process.rs`)
> skips `JsonPatch`, keeping only `Stdout`/`Stderr`. So a query for that marker
> in persisted logs can never match. Instead, count consecutive prior processes
> whose durably-persisted `executor_action` carries the fixed auto-continue
> prompt sentinel — that column IS stored. This needs no new query and no new
> `sqlx` dependency; iterate the already-fetched `ExecutionProcess` list.

Define the shared sentinel constant at module scope in `container.rs` (Task 6's
`start_auto_continue` uses the same constant, so the prompts match exactly):

```rust
/// Prompt sent when auto-continuing a Claude turn interrupted by a transient
/// API stall. Also the durable marker used to count consecutive auto-continues.
pub(crate) const AUTO_CONTINUE_PROMPT: &str = "Please continue from where you were interrupted.";
```

```rust
/// True if this process is an auto-continue follow-up (its executor action is a
/// coding-agent follow-up carrying the `AUTO_CONTINUE_PROMPT` sentinel).
fn is_auto_continue_process(proc: &ExecutionProcess) -> bool {
    proc.executor_action()
        .ok()
        .map(|action| {
            matches!(
                action.typ(),
                ExecutorActionType::CodingAgentFollowUpRequest(req)
                    if req.prompt == AUTO_CONTINUE_PROMPT
            )
        })
        .unwrap_or(false)
}

/// How many of the most-recent *consecutive* coding-agent processes for this
/// session (excluding `before_exec_id`) were auto-continue follow-ups. Used to
/// cap auto-continue depth. Stateless / restart-safe: reads the persisted
/// `executor_action` column, not derived log entries.
async fn consecutive_auto_continue_count(&self, session_id: Uuid, before_exec_id: Uuid) -> u32 {
    let procs = match ExecutionProcess::find_by_session_id(&self.db.pool, session_id, false).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("consecutive_auto_continue_count query failed: {e}");
            return u32::MAX; // fail closed: treat as "at cap", do not retry
        }
    };
    let mut count = 0;
    // find_by_session_id returns ASC by created_at; walk newest-first.
    for p in procs.iter().rev() {
        if p.id == before_exec_id {
            continue;
        }
        if p.run_reason != ExecutionProcessRunReason::CodingAgent {
            break;
        }
        if is_auto_continue_process(p) {
            count += 1;
        } else {
            break;
        }
    }
    count
}
```

Add imports as needed: `ExecutionProcessRunReason` (already imported at
`container.rs:17`) and `ExecutorActionType` (from the `executors::actions`
module — check the existing `use` for `CodingAgentFollowUpRequest` at
`container.rs:32` and add `ExecutorActionType` alongside it). Do **not** add a
raw `sqlx` dependency to this crate.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p local-deployment detects_retryable_stall_in_history no_retryable_stall_in_empty_history`
Expected: PASS.
Run: `cargo check -p local-deployment`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/local-deployment/src/container.rs
git commit -m "feat(deployment): detect retryable Claude stalls and count auto-continues"
```

---

### Task 6: Trigger auto-continue in the exit monitor

**Files:**
- Modify: `crates/local-deployment/src/container.rs:582-627` (the failure/finalize region inside `spawn_exit_monitor`) and add an `async fn start_auto_continue` near `start_queued_follow_up` (line 1209).

**Interfaces:**
- Consumes: `last_turn_ended_in_retryable_stall`, `consecutive_auto_continue_count` (Task 5); `Config.auto_continue_on_api_stall_enabled` (Task 4); `CodingAgentFollowUpRequest`, `start_execution`, `CodingAgentTurn::find_latest_session_info` (existing, `container.rs:1209-1279`).
- Produces: side effect — spawns a resumed coding-agent follow-up instead of finalizing.

- [ ] **Step 1: Add `start_auto_continue`**

Add this method to the same `impl LocalContainerService` block that holds `start_queued_follow_up` (`container.rs`, before line 1280). It mirrors `start_queued_follow_up` but uses the fixed continue prompt and the session's default executor config:

```rust
async fn start_auto_continue(
    &self,
    ctx: &ExecutionContext,
) -> Result<ExecutionProcess, ContainerError> {
    // AUTO_CONTINUE_PROMPT is the shared module-level const defined in Task 5 —
    // the counter reads it to identify prior auto-continues, so both must use
    // the exact same string. Do NOT redefine it locally.

    let latest_session_info =
        CodingAgentTurn::find_latest_session_info(&self.db.pool, ctx.session.id).await?;
    let Some(info) = latest_session_info else {
        return Err(ContainerError::Other(anyhow!(
            "cannot auto-continue: no prior session info for session {}",
            ctx.session.id
        )));
    };

    let executor_config = info.executor_profile_id.clone();

    let repos = WorkspaceRepo::find_repos_for_workspace(&self.db.pool, ctx.workspace.id).await?;
    let cleanup_action = self.cleanup_actions_for_repos(&repos);
    let working_dir = ctx
        .session
        .agent_working_dir
        .as_ref()
        .filter(|dir| !dir.is_empty())
        .cloned();

    let action_type = ExecutorActionType::CodingAgentFollowUpRequest(CodingAgentFollowUpRequest {
        prompt: AUTO_CONTINUE_PROMPT.to_string(),
        session_id: info.session_id,
        reset_to_message_id: None,
        executor_config,
        working_dir,
    });
    let action = ExecutorAction::new(action_type, cleanup_action.map(Box::new));

    self.start_execution(
        &ctx.workspace,
        &ctx.session,
        &action,
        &ExecutionProcessRunReason::CodingAgent,
    )
    .await
}
```

Note: confirm the exact shape of `find_latest_session_info`'s return (`container.rs:1240-1257` uses `info.session_id`). If it does not carry an executor profile, obtain the executor config the same way `start_queued_follow_up` derives `expected_executor` — via `ExecutionProcess::latest_executor_profile_for_session(&self.db.pool, ctx.session.id)` — and use that as `executor_config`. Use whichever of these two the type actually provides; do not invent a field.

- [ ] **Step 2: Wire the trigger into the failure path**

In `spawn_exit_monitor`, the block currently at `container.rs:582-627` handles the `success || cleanup_done` path and otherwise falls through to finalize (`should_finalize` at line 629). Insert an auto-continue guard so that a failed coding-agent run with a retryable stall resumes instead of finalizing. Immediately **before** the `if !already_finalized && container.should_finalize(&ctx)` block (line 629), add:

```rust
// Auto-continue transient Claude API stalls (e.g. "Response stalled mid-stream").
// Detect Claude via the real accessor `ExecutorAction::base_executor()`
// (crates/executors/src/actions/mod.rs:62); `BaseCodingAgent::ClaudeCode` is
// already used elsewhere in this file (~container.rs:1552).
let is_claude = ctx
    .execution_process
    .executor_action()
    .ok()
    .and_then(|a| a.base_executor())
    == Some(BaseCodingAgent::ClaudeCode);

let auto_continue_enabled = { config.read().await.auto_continue_on_api_stall_enabled };

if !already_finalized
    && auto_continue_enabled
    && is_claude
    && matches!(
        ctx.execution_process.run_reason,
        ExecutionProcessRunReason::CodingAgent
    )
    && matches!(ctx.execution_process.status, ExecutionProcessStatus::Failed)
    && container.last_turn_ended_in_retryable_stall(&exec_id)
{
    // `consecutive_auto_continue_count` counts auto-continues STRICTLY BEFORE
    // `exec_id`. The just-failed process itself is an auto-continue from the
    // 2nd stall onward, so it must be counted toward the budget too — otherwise
    // the chain overshoots the cap by one (4 spawned instead of 3).
    let prior = container
        .consecutive_auto_continue_count(ctx.session.id, exec_id)
        .await;
    let attempts_so_far = prior.saturating_add(
        if is_auto_continue_process(&ctx.execution_process) { 1 } else { 0 },
    );
    if under_retry_cap(attempts_so_far) {
        tracing::info!(
            "Auto-continuing session {} after transient Claude API stall (attempt {} of {})",
            ctx.session.id,
            attempts_so_far + 1,
            MAX_AUTO_CONTINUE_RETRIES
        );
        tokio::time::sleep(AUTO_CONTINUE_BACKOFF).await;
        match container.start_auto_continue(&ctx).await {
            Ok(_) => {
                already_finalized = true; // a new run is now in flight; do not finalize
            }
            Err(e) => {
                tracing::error!("Failed to start auto-continue for session {}: {e}", ctx.session.id);
                // fall through to normal finalize
            }
        }
    } else {
        tracing::warn!(
            "Session {} hit auto-continue cap ({}); finalizing as failed",
            ctx.session.id,
            MAX_AUTO_CONTINUE_RETRIES
        );
    }
}
```

Add the constants near the top of `container.rs` (module level):

```rust
const MAX_AUTO_CONTINUE_RETRIES: u32 = 3;
const AUTO_CONTINUE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);
```

Notes:
- `already_finalized` is a `let mut` already declared at `container.rs:582`; setting it `true` here prevents the subsequent `should_finalize` finalize block from running, matching how the success path uses it.
- `config` is already captured into the exit-monitor task (`container.rs:497`), of type `Arc<RwLock<Config>>`.
- Executor-kind detection: use `ExecutorAction::base_executor() -> Option<BaseCodingAgent>` (crates/executors/src/actions/mod.rs:62), compared to `Some(BaseCodingAgent::ClaudeCode)`. `BaseCodingAgent` is already imported at `container.rs:37`. Task 6 also introduces the `under_retry_cap(attempts: u32) -> bool { attempts < MAX_AUTO_CONTINUE_RETRIES }` helper used by the guard (Task 7 tests it).

- [ ] **Step 3: Build**

Run: `cargo check -p local-deployment`
Expected: PASS. Fix any accessor-name mismatches per the notes above (do not invent methods — grep for the real ones).

- [ ] **Step 4: Commit**

```bash
git add crates/local-deployment/src/container.rs
git commit -m "feat(deployment): auto-continue Claude turns after transient API stalls"
```

---

### Task 7: Integration-ish test for the retry cap

**Files:**
- Modify: `crates/local-deployment/src/container.rs` (`#[cfg(test)]` block)

**Interfaces:**
- Consumes: `under_retry_cap` and `MAX_AUTO_CONTINUE_RETRIES` (defined in Task 6's guard fix).

Task 6's fix already defines `fn under_retry_cap(attempts: u32) -> bool { attempts < MAX_AUTO_CONTINUE_RETRIES }` and uses it in the guard over `attempts_so_far` (the consecutive-count PLUS the just-failed process if it is itself an auto-continue). This task adds a test that pins the exact cap boundary, guarding the off-by-one that a prior review caught (the chain must stop at 3 total auto-continues, not 4).

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/local-deployment/src/container.rs`:

```rust
#[test]
fn cap_allows_three_then_blocks_fourth() {
    // `attempts_so_far` = auto-continues already spawned in this chain,
    // including the just-failed one when it is itself an auto-continue.
    // Original stall: 0 prior → allowed (spawns #1).
    assert!(under_retry_cap(0));
    // After #1 failed: 1 → allowed (spawns #2).
    assert!(under_retry_cap(1));
    // After #2 failed: 2 → allowed (spawns #3).
    assert!(under_retry_cap(2));
    // After #3 failed: 3 → BLOCKED (must NOT spawn a 4th).
    assert!(!under_retry_cap(3));
    assert!(!under_retry_cap(4));
}
```

- [ ] **Step 2: Run test to verify it passes (helper already exists from Task 6)**

Run: `cargo test -p local-deployment cap_allows_three_then_blocks_fourth`
Expected: PASS. (If `under_retry_cap` is somehow missing, add `fn under_retry_cap(attempts: u32) -> bool { attempts < MAX_AUTO_CONTINUE_RETRIES }` at module scope and wire the guard to use it, then re-run.)

Note: `under_retry_cap` is defined by Task 6, so this test may pass immediately (TDD red-first does not strictly apply to a pure boundary assertion over an existing predicate). Its value is regression-pinning the off-by-one boundary. Confirm it genuinely exercises the real `under_retry_cap` (not a shadow copy).

- [ ] **Step 5: Commit**

```bash
git add crates/local-deployment/src/container.rs
git commit -m "test(deployment): cover auto-continue retry cap"
```

---

### Task 8: Final verification

- [ ] **Step 1: Format**

Run: `pnpm run format`

- [ ] **Step 2: Full backend checks + tests**

Run: `pnpm run backend:check`
Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 3: Frontend/type check**

Run: `pnpm run check`
Expected: PASS (confirms regenerated `shared/types.ts` is consistent).

- [ ] **Step 4: Commit any formatting changes**

```bash
git add -A
git commit -m "chore: format auto-continue changes"
```

---

## Self-Review

**Spec coverage:**
- Detection/classification → Task 2. ✅
- Structured error entry for all strategies + `RetryableApiError` variant → Tasks 1, 3. ✅
- Trigger in exit monitor → Task 6. ✅
- Resume + nudge follow-up → Task 6 (`start_auto_continue`). ✅
- Stateless consecutive-count cap (3) → Tasks 5, 7. ✅
- Config flag (default on) → Task 4. ✅
- Backoff (~2s) → Task 6 constant. ✅
- Claude-only → Task 6 `is_claude` guard. ✅
- Testing (classifier, cap, recovered-case untouched) → Tasks 2, 5, 7; recovered case never reaches the failure path (documented in design). ✅

**Placeholder scan:** The only deliberately open items are accessor-name confirmations in Tasks 6 (`coding_agent_profile_id` / `find_latest_session_info` shape), flagged with a concrete fallback (`latest_executor_profile_for_session`) rather than a bare TODO — the implementer greps for the real name. Task 7 offers a concrete fallback test when no DB harness exists.

**Type consistency:** `NormalizedEntryError::RetryableApiError`, `classify_claude_api_error`, `ClaudeApiErrorClass`, `history_has_retryable_stall`, `last_turn_ended_in_retryable_stall`, `consecutive_auto_continue_count`, `start_auto_continue`, `under_retry_cap`, `MAX_AUTO_CONTINUE_RETRIES`, `AUTO_CONTINUE_BACKOFF`, `auto_continue_on_api_stall_enabled` — used consistently across tasks.
