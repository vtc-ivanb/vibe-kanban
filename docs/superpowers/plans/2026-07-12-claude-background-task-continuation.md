# Claude Background-Task Continuation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep the Claude SDK session alive across `run_in_background` shell tasks so the *same* session continues automatically when the task completes, instead of tearing down at the first `Result` and losing the notification.

**Architecture:** vibe-kanban runs Claude as a persistent SDK session over stdin/stdout (`ProtocolPeer::read_loop`). Today the loop `break`s on the first `Result`, closing stdin and killing the background child. We add read-loop-local tracking of outstanding background tasks keyed by `task_id` (from `system`/`task_started` and `task_notification` messages); a `Result` is only final when the outstanding set is empty. While waiting we keep the process alive and Claude's own harness delivers the completion notification and **auto-continues** (spike-confirmed). We also fix a `Stop`-hook teardown race that makes follow-ups unrecoverable.

**Tech Stack:** Rust (tokio), `crates/executors` (Claude executor + control protocol), `crates/local-deployment` (process lifecycle), `serde_json`.

## Global Constraints

- Claude executor only. Background **shell commands** (`run_in_background: true` Bash) — which the spike showed surface via the generic `task_started`/`task_notification` signals, so background subagents are covered by the same mechanism for free. Plain `cmd &` and other executors are out of scope.
- **Wait is unbounded** — no timeout. Only a manual stop ends a waiting session.
- Do **not** add Claude co-author / "Generated with Claude Code" footers to commits (project rule). Use conventional-commit messages (`type(scope): …`).
- Run `pnpm run format` before finishing; `cargo test --workspace` must pass.
- No new DB status variant — surface "waiting" as a normalized system message, not an `ExecutionProcessStatus` change.

## Spike outcome (drives the concrete tasks below)

Captured stream order (`crates/executors/src/executors/claude/fixtures/bg_bash_stream.jsonl`):
`assistant(Bash bg)` → `system/task_started(task_id)` → `result` **← today we wrongly break here** → `system/task_updated` → `system/task_notification(task_id, status:"completed")` → `result` (true final). Auto-continue is automatic once stdin stays open; **no nudge injection needed**.

---

## File Structure

- `crates/executors/src/executors/claude/background.rs` (**new**) — pure helper `apply_task_event` that updates an outstanding `HashSet<String>` of `task_id`s from a parsed stream line. Unit-tested in isolation against the fixture.
- `crates/executors/src/executors/claude/mod.rs` — register `mod background;`.
- `crates/executors/src/executors/claude/protocol.rs` — read loop keeps the session alive while the set is non-empty; announces waiting; graceful teardown; `is_final_result` helper.
- `crates/executors/src/executors/claude.rs` — normalization test that the waiting status line renders as a `SystemMessage`.
- `crates/executors/src/executors/claude/client.rs` — teardown-race fix around the `Stop`-hook round-trip (guided by the Task 1 repro).

---

## Task 1: Spike — DONE ✅

Findings recorded in `docs/superpowers/specs/2026-07-12-…-design.md` ("## Spike findings"); fixture saved to `crates/executors/src/executors/claude/fixtures/bg_bash_stream.jsonl`. Auto-continue confirmed; tracking keyed by `task_id` via `task_started`/`task_notification`. Committed with the spec update.

---

## Task 2: Background-task tracking + read-loop keep-alive

**Files:**
- Create: `crates/executors/src/executors/claude/background.rs`
- Modify: `crates/executors/src/executors/claude/mod.rs` (add `mod background;`)
- Modify: `crates/executors/src/executors/claude/protocol.rs` (read loop)
- Test: `crates/executors/src/executors/claude/background.rs` (`#[cfg(test)]`, uses the fixture)

**Interfaces:**
- Produces: `pub(crate) fn apply_task_event(outstanding: &mut std::collections::HashSet<String>, line: &serde_json::Value)` — on `system`/`task_started` with a `task_id`, insert it; on `system`/`task_notification` with a `task_id`, remove it; ignore everything else (incl. `task_updated`).
- Produces: `pub(crate) fn is_final_result(outstanding: &std::collections::HashSet<String>) -> bool` (in `protocol.rs`) — `outstanding.is_empty()`.

- [ ] **Step 1: Write the failing tests** (`background.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use serde_json::json;

    #[test]
    fn task_started_inserts_and_notification_removes() {
        let mut out: HashSet<String> = HashSet::new();
        apply_task_event(&mut out, &json!({"type":"system","subtype":"task_started","task_id":"t1"}));
        assert_eq!(out.len(), 1);
        // progress event does not change the set
        apply_task_event(&mut out, &json!({"type":"system","subtype":"task_updated","task_id":"t1"}));
        assert_eq!(out.len(), 1);
        apply_task_event(&mut out, &json!({"type":"system","subtype":"task_notification","task_id":"t1","status":"completed"}));
        assert!(out.is_empty());
    }

    #[test]
    fn unrelated_lines_are_ignored() {
        let mut out: HashSet<String> = HashSet::new();
        apply_task_event(&mut out, &json!({"type":"result","subtype":"success"}));
        apply_task_event(&mut out, &json!({"type":"assistant","message":{"role":"assistant","content":[]}}));
        assert!(out.is_empty());
    }

    #[test]
    fn full_captured_stream_returns_to_empty_with_late_completion() {
        // The fixture is ordered: bash → task_started → result#1 → task_updated
        // → task_notification → result#2. The set must be NON-empty at result#1
        // and empty only after the task_notification.
        let raw = include_str!("fixtures/bg_bash_stream.jsonl");
        let mut out: HashSet<String> = HashSet::new();
        let mut seen_first_result = false;
        let mut nonempty_at_first_result = false;
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if v.get("type").and_then(|t| t.as_str()) == Some("result") && !seen_first_result {
                seen_first_result = true;
                nonempty_at_first_result = !out.is_empty();
            }
            apply_task_event(&mut out, &v);
        }
        assert!(nonempty_at_first_result, "background task must still be outstanding at the first result");
        assert!(out.is_empty(), "set must be empty after the completion notification");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p executors -- background::tests`
Expected: FAIL (module/functions not found).

- [ ] **Step 3: Implement the helper** (`background.rs`)

```rust
use serde_json::Value;
use std::collections::HashSet;

/// Update the set of outstanding background `task_id`s from a single stream line.
///
/// Background work (both `run_in_background` bash and async subagents) surfaces as
/// `system` messages: `task_started` when it launches and `task_notification` when it
/// reaches a terminal state. `task_updated` is progress only and is ignored. Keying by
/// `task_id` keeps concurrent tasks independent and is self-correcting for foreground
/// tasks that start and finish within one turn.
pub(crate) fn apply_task_event(outstanding: &mut HashSet<String>, line: &Value) {
    if line.get("type").and_then(|t| t.as_str()) != Some("system") {
        return;
    }
    let Some(task_id) = line.get("task_id").and_then(|t| t.as_str()) else {
        return;
    };
    match line.get("subtype").and_then(|s| s.as_str()) {
        Some("task_started") => {
            outstanding.insert(task_id.to_string());
        }
        Some("task_notification") => {
            outstanding.remove(task_id);
        }
        _ => {}
    }
}
```

Register the module: add `mod background;` to `claude/mod.rs`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p executors -- background::tests`
Expected: PASS.

- [ ] **Step 5: Add the `is_final_result` helper + test** (`protocol.rs`)

```rust
pub(crate) fn is_final_result(outstanding: &std::collections::HashSet<String>) -> bool {
    outstanding.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    #[test]
    fn result_final_only_when_no_outstanding() {
        let mut out = HashSet::new();
        assert!(is_final_result(&out));
        out.insert("t1".to_string());
        assert!(!is_final_result(&out));
    }
}
```

- [ ] **Step 6: Wire into the read loop** (`protocol.rs`). Add local state near `interrupt_sent`:

```rust
        let mut interrupt_sent = false;
        let mut outstanding: std::collections::HashSet<String> = std::collections::HashSet::new();
```

Inside the `Ok(_) =>` arm, after `client.log_message(line).await?;` and before the `CLIMessage` match, update the set:

```rust
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                                super::background::apply_task_event(&mut outstanding, &v);
                            }
```

Change the `Result` arm to only end the turn when nothing is outstanding:

```rust
                                Ok(CLIMessage::Result(_)) => {
                                    if is_final_result(&outstanding) {
                                        break;
                                    }
                                    // else: background task(s) still running — keep the
                                    // session alive and keep reading. Claude's harness fires
                                    // task_notification on completion and auto-continues.
                                }
```

- [ ] **Step 7: Run tests + clippy**

Run: `cargo test -p executors -- background:: protocol::` then `cargo clippy -p executors`
Expected: PASS, no new warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/executors/src/executors/claude/background.rs \
  crates/executors/src/executors/claude/mod.rs \
  crates/executors/src/executors/claude/protocol.rs
git commit -m "feat(executors): keep Claude session alive while background tasks run"
```

---

## Task 3: Surface a "waiting on background task" marker

**Files:**
- Modify: `crates/executors/src/executors/claude/protocol.rs` (announce once on entering wait)
- Test: `crates/executors/src/executors/claude.rs` (`#[cfg(test)]`, normalization)

**Interfaces:**
- Consumes: `client.log_message` (existing `LogWriter` path).
- Produces: a `{"type":"system","subtype":"status","status":"⏳ Waiting for background task to finish…"}` line when the session first parks on a non-final `Result`. `ClaudeLogProcessor` already maps `subtype:"status"` → `SystemMessage` (`claude.rs:1274-1278`), so it renders with no parser change.

- [ ] **Step 1: Write the failing test** (in `claude.rs` tests; mirror an existing `test_system_message_*` test for the helper it uses)

```rust
#[test]
fn test_waiting_status_renders_as_system_message() {
    let json = r#"{"type":"system","subtype":"status","status":"⏳ Waiting for background task to finish…"}"#;
    let parsed: ClaudeJson = serde_json::from_str(json).unwrap();
    let patches = process_single_for_test(&parsed); // reuse the file's existing test harness
    // one of the produced entries is a SystemMessage containing the waiting text
    assert!(patches.iter().any(|e|
        matches!(e.entry_type, NormalizedEntryType::SystemMessage)
            && e.content.contains("Waiting for background task")));
}
```

(If the file's tests use a different harness name than `process_single_for_test`, use that one — match the existing `Some("status")` system-message test.)

- [ ] **Step 2: Run to verify it fails or passes**

Run: `cargo test -p executors test_waiting_status_renders_as_system_message`
Expected: PASS of normalization (guard that the chosen shape renders) — if the harness name is wrong it FAILs to compile; fix to the real helper.

- [ ] **Step 3: Announce once on entering wait** (`protocol.rs`). Track a flag so repeated non-final `Result`s don't spam, and reset it when the set empties:

```rust
        let mut waiting_announced = false;
```

In the non-final `Result` branch (the `else` of `is_final_result`):

```rust
                                    if !waiting_announced {
                                        waiting_announced = true;
                                        let _ = client
                                            .log_message(r#"{"type":"system","subtype":"status","status":"⏳ Waiting for background task to finish…"}"#)
                                            .await;
                                    }
```

After the `apply_task_event` call, reset when drained so a later task re-announces:

```rust
                            if outstanding.is_empty() {
                                waiting_announced = false;
                            }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p executors test_waiting_status_renders_as_system_message`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/executors/src/executors/claude/protocol.rs crates/executors/src/executors/claude.rs
git commit -m "feat(executors): show waiting-on-background-task status in Claude session"
```

---

## Task 4: `Stop`-hook teardown race — INVESTIGATED, no fix warranted

**Outcome (2026-07-13):** Reproduced against real `claude` 2.1.198 with the `Stop`
git-check hook registered via the control protocol (`/tmp/vk-spike/hook_driver.py`):

- **Keep-alive (our fix):** session stays alive past `result#1`, receives
  `task_notification(completed)`, auto-continues, answers the final Stop hook, emits
  `result#2`, and tears down cleanly — **no `Stream closed` crash**.
- **Close-on-first-result (old behavior):** closing stdin just stops the background task
  (`task_notification status:"stopped"`) and exits 0 — **also no crash**.

The isolated `Stream closed at sendRequest` crash did **not** reproduce from a stdin close.
The read loop already awaits `handle_control_request` **inline** (`protocol.rs:105`), so the
Stop-hook response is always flushed before the subsequent `Result` is processed — there is
no hook-response-vs-teardown race in the read loop to fix. The user's crash needs a more
specific trigger (likely `kill_process_group`'s SIGINT/SIGTERM sequencing or `--resume`
timing) that could not be reproduced. Per systematic-debugging, no speculative fix/test is
added. Our keep-alive change (Task 2) removes the premature-teardown condition that the
observed continuation flow depended on. **Task closed as investigated; reopen only if a
concrete repro surfaces.**

<details><summary>Original plan (superseded)</summary>

Reproduce with the hook registered, then guard the ordering.

**Files:**
- Modify: `crates/executors/src/executors/claude/protocol.rs` (settle pending hook round-trips before dropping stdin)
- Test: `crates/executors/src/executors/claude/protocol.rs` (`#[cfg(test)]`)

- [ ] **Step 1: Reproduce with the hook registered.** Extend the spike driver to send an `initialize` control_request registering `Stop → [STOP_GIT_CHECK]` and to answer the resulting hook `control_request`, then close stdin mid-round-trip and on a `--resume`. Confirm the `Stream closed at sendRequest` ordering. Record the triggering sequence in a comment on the test below.

- [ ] **Step 2: Write the failing test.** Drive `ProtocolPeer` over an in-memory duplex: feed `{control_request: Stop hook}` then `{result}` then EOF; assert a `control_response` for the hook `request_id` is written to stdin **before** the loop returns on the final `Result`.

```rust
    #[tokio::test]
    async fn stop_hook_response_flushed_before_teardown() {
        // Sequence that triggers the crash (from Step 1):
        //   Claude -> control_request(Stop hook)  ; SDK must answer
        //   Claude -> result                       ; do NOT close read side until the
        //                                            hook response has been flushed
        // Build peer over tokio::io::duplex, feed the two lines, then assert the
        // hook response was written prior to loop return.
    }
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p executors stop_hook_response_flushed_before_teardown`
Expected: FAIL (response raced/dropped by the break).

- [ ] **Step 4: Implement the ordering fix** per Step 1's finding. Ensure that when `is_final_result` is true, any in-flight hook-callback task has flushed its `send_hook_response` before the loop returns (drop stdin). If the repro shows the crash is Claude-side (it sends after our read end is gone), keep the reader draining until Claude's own post-hook `Result` rather than returning on the first — do not close the read side while a hook response is pending. The test pins the required behavior.

- [ ] **Step 5: Run tests**

Run: `cargo test -p executors -- protocol::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/executors/src/executors/claude/protocol.rs
git commit -m "fix(executors): flush Stop-hook response before session teardown"
```

</details>

---

## Task 5: Prompt stop while parked + regression sweep

**Finding:** `stop_execution` (`container.rs:1577`) already sets status, cancels the token,
waits ≤5s for graceful exit, then unconditionally `kill_process_group`s the whole group
(`container.rs:1620`) — so manual stop already works while parked, and the background child
(in the group) is killed. **Gap introduced by Task 2:** on cancel, the read loop sends
`interrupt`, Claude emits a `Result`, but with a background task still outstanding our loop
would *not* break — so stop would wait the full 5s force-kill window instead of ending
gracefully. Fix: once interrupted, any `Result` ends the loop.

**Files:**
- Modify: `crates/executors/src/executors/claude/protocol.rs` (`should_end_turn`)
- Test: `crates/executors/src/executors/claude/protocol.rs` (`#[cfg(test)]`)

- [x] **Step 1: Replace `is_final_result(&outstanding)` with `should_end_turn(interrupted, &outstanding)`** — `interrupted || outstanding.is_empty()` — and pass `interrupt_sent` at the call site so a stop request ends the turn on the next `Result`.

```rust
pub(crate) fn should_end_turn(
    interrupted: bool,
    outstanding: &std::collections::HashSet<String>,
) -> bool {
    interrupted || outstanding.is_empty()
}
```

- [x] **Step 2: Unit tests** — `should_end_turn(false, {t1}) == false`, `should_end_turn(true, {t1}) == true`, `should_end_turn(false, {}) == true`.

Run: `cargo test -p executors -- protocol::` → PASS.

- [ ] **Step 3: Manual e2e** against a real worktree (spike harness or the running app): ask Claude to run a ~60s background command and end its turn; confirm (a) the execution stays `Running` with the waiting marker, (b) the *same* session continues and commits after completion, (c) a manual stop mid-wait ends promptly.

- [ ] **Step 4: Format, full test, commit**

```bash
pnpm run format
cargo test --workspace
git add -A
git commit -m "feat(executors): end parked Claude session promptly on stop"
```

---

## Self-Review

**Spec coverage:**
- Keep session alive across background tasks → Task 2. ✓
- Track outstanding by `task_id` (task_started/notification) → Task 2. ✓
- Finalize only on final `Result` → Task 2 (`is_final_result`). ✓
- Unbounded wait, manual stop → Task 5. ✓
- Waiting-state visibility → Task 3. ✓
- `Stop`-hook teardown race → Task 4. ✓
- Auto-continue confirmed (no nudge) → Task 1 findings; Task 2 relies on it. ✓
- Process-group backstop: covered by Task 5's kill-through on stop; the spec's counter-reconciliation backstop is YAGNI given the `task_notification` signal proved reliable — add only if a future case shows a missed notification.

**Placeholder scan:** No spike placeholders remain — Task 1 resolved the completion signal, so Tasks 2–3 are fully concrete. The only deferred detail is Task 4's exact fix branch, which legitimately depends on the Step 1 hook repro and is pinned by the Step 2 test.

**Type consistency:** `apply_task_event(&mut HashSet<String>, &Value)` and `is_final_result(&HashSet<String>) -> bool` used consistently across Tasks 2–5.
