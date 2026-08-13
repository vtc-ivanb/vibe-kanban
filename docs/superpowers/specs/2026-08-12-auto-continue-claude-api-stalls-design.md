# Auto-continue Claude turns interrupted by transient API errors

**Date:** 2026-08-12
**Status:** Approved (design)
**Scope:** Claude executor only

## Problem

When the Anthropic API's streaming response goes quiet mid-turn, the `claude`
CLI (which the executor spawns in `--print --output-format stream-json` mode)
gives up on the turn and emits an error such as:

> API Error: Response stalled mid-stream. The response above may be incomplete.

The CLI already retries internally before surfacing this, so vibe-kanban cannot
*prevent* the stall — it is an Anthropic-API-layer event. What we can do is
**automatically recover**: detect the interrupted turn and resume it, the way a
user would by manually typing "continue".

## How the failure manifests

A terminal `result` line from the CLI looks like this (real capture of a 401,
which has the same shape a stall produces):

```json
{"type":"result","subtype":"success","is_error":true,
 "result":"Failed to authenticate. API Error: 401 ...",
 "stop_reason":"stop_sequence","num_turns":1, ...}
```

Key points:
- `subtype` is still `"success"`, but **`is_error: true`**.
- The process then exits **non-zero**, so vibe-kanban marks the execution
  process **`failed`** (`container.rs` `spawn_exit_monitor`).
- A contrasting case exists where "API Error" appears mid-stream but the CLI
  recovers on its own and the run finishes `completed / exit 0` — those must not
  be touched.

The executor already parses `is_error` on the `Result` message
(`crates/executors/src/executors/claude.rs:1798`), but today only the
`AmpResume` history strategy turns it into a structured error entry
(`claude.rs:1826-1844`). A normal Claude run leaves the stall text as a stray
assistant message or nothing.

## Design

### 1. Classify terminal errors (Claude executor)

Add a classifier that, given a terminal `result` with `is_error: true` (and/or
the preceding assistant `error` field), returns:

- **Retryable** — text matches: `stalled mid-stream` / `Response stalled`,
  `overloaded` / `529`, `500` / `internal server error`,
  request timeout, connection error.
- **Fatal** — text matches: `401` / `403` / `authentication_error` /
  `OAuth token has expired`, `400` / `invalid_request`, `prompt is too long`,
  billing / credit-balance errors. Anything unrecognized defaults to **Fatal**
  (safer — no retry loop on unknown errors).

The classifier lives in the Claude executor module and is unit-tested against
real JSON.

### 2. Emit a structured error entry (all Claude strategies)

Currently only `AmpResume` emits an `ErrorMessage` entry for `is_error`
results. Extend this so **every** Claude strategy emits an `ErrorMessage`
normalized entry when a terminal `is_error` result arrives. The entry carries a
new error classification:

- Extend `NormalizedEntryError` (`crates/executors/src/logs/mod.rs:65`, today
  `SetupRequired | Other`) with a **`RetryableApiError`** variant. Fatal API
  errors continue to use `Other`.

This is a tagged enum consumed by ts-rs, so `pnpm run generate-types` must be
re-run. Side benefit: the UI now renders a real error entry for these turns.

### 3. Trigger auto-continue (container exit monitor)

In `spawn_exit_monitor` (`crates/local-deployment/src/container.rs:488`), on the
failure path and **before** `finalize_task`, add a guard that fires only when
**all** hold:

1. `run_reason == CodingAgent`
2. status is `Failed`
3. the config flag (below) is enabled
4. the run's MsgStore history contains a `RetryableApiError` entry — detected
   with a new helper modeled on `extract_last_assistant_message`
   (`container.rs:946`) that scans history patches for the entry type
5. the consecutive-retry count (below) is `< MAX_AUTO_CONTINUE_RETRIES` (3)

When the guard fires, spawn an auto-continue follow-up instead of finalizing.
Otherwise fall through to the existing finalize logic unchanged.

### 4. Auto-continue = resume + nudge

Build a `CodingAgentFollowUpRequest` exactly as `start_queued_follow_up` does
(`container.rs:1209`): resume the latest session
(`CodingAgentTurn::find_latest_session_info`) with a short prompt:

> Please continue from where you were interrupted.

This reuses the executor's `spawn_follow_up` `--resume <session_id>` path. A
brief backoff (~2s) before spawning lets the transient server condition clear.

The follow-up is a **normal, visible** coding-agent turn: the `RetryableApiError`
entry from the interrupted turn shows first, then the continue prompt, so the
thread reads clearly. No new "invisible message" machinery.

### 5. Bounding the loop (stateless)

No persistent counter. Immediately before retrying, count the **consecutive**
most-recent coding-agent execution processes for this session that ended in a
`RetryableApiError`. If the count is `>= 3`, stop and finalize as failed.

Rationale: each retry that fails again produces another stall-terminated
process, so the consecutive chain measures retry depth directly. It survives app
restarts and needs no schema change. A successful or normally-failing turn
breaks the chain and resets the effective count to zero.

### 6. Config

Add to `Config` (`crates/services/src/services/config/versions/v8.rs:40`):

```rust
#[serde(default = "default_auto_continue_on_api_stall_enabled")]
pub auto_continue_on_api_stall_enabled: bool, // default true
```

Additive, defaulted — follows the existing `#[serde(default)]` flag convention
(e.g. `commit_reminder_enabled`), no migration required. The container reads it
before retrying, giving users a kill switch.

## Constants

- `MAX_AUTO_CONTINUE_RETRIES = 3`
- `AUTO_CONTINUE_BACKOFF = ~2s`

## Testing

- **Executor unit tests** for the classifier: the captured 401 result → Fatal;
  a synthesized `stalled mid-stream` result → Retryable; assert the emitted
  `NormalizedEntryError` variant for each.
- **Container test** for the consecutive-count cap: retryable stalls chain and
  auto-continue fires until the 3rd, then finalizes as failed rather than
  retrying a 4th time.
- Confirm the mid-stream-but-recovered case (`completed / exit 0`) never triggers
  a retry (it never reaches the failure path).

## Out of scope

- Other executors (Codex, Gemini, etc.) — this is Claude-only for now. The
  classifier and trigger are structured so a future change could generalize
  them, but no such abstraction is built now.
- Preventing the stall itself (not controllable from vibe-kanban).
- Any UI beyond the error entry that already falls out of step 2.

## Files touched (anticipated)

- `crates/executors/src/executors/claude.rs` — classifier + emit `ErrorMessage`
  for all strategies.
- `crates/executors/src/logs/mod.rs` — `NormalizedEntryError::RetryableApiError`.
- `crates/local-deployment/src/container.rs` — detection helper, retry trigger,
  consecutive-count cap.
- `crates/services/src/services/config/versions/v8.rs` — config flag.
- `shared/types.ts` — regenerated via `pnpm run generate-types`.
