# Claude background-task continuation

**Status:** Design
**Date:** 2026-07-12
**Scope:** Claude executor only. Background *shell commands* (`run_in_background: true` Bash). Background subagents are out of scope for v1.

## Problem

When the Claude agent runs a long shell command in the background and ends its turn
expecting to be re-invoked when it finishes ("I'll wait for the `onnx` install to
complete — the harness will notify"), vibe-kanban never continues the session. The
background task's result is lost and, in the observed incident, the session becomes
unrecoverable.

### Why it happens

vibe-kanban runs Claude as a **persistent SDK session** over stdin/stdout using the
control protocol (`crates/executors/src/executors/claude/protocol.rs`). The session is
driven entirely by the client: Claude stays alive as long as its stdin is open and
continues to emit turns.

The read loop, however, treats the **first** `Result` message as the definitive end of
the turn and `break`s (`protocol.rs:90-92`). Breaking drops the last `ChildStdin`
reference, closing Claude's stdin; Claude, running in `-p` stream-json mode, then exits
on stdin EOF. Because a `run_in_background` shell is a child of the Claude process, it is
killed/orphaned when Claude exits, and any completion notification Claude *would* have
delivered on a later turn reaches a dead process.

Completion is detected purely via OS process-group exit
(`crates/local-deployment/src/container.rs:841-899`, `spawn_os_exit_watcher`); Claude
returns `exit_signal: None` (`claude.rs:708-712`). Nothing keeps the session alive across
a background task, and nothing re-invokes it on completion. Session continuation only ever
happens via a **new** `--resume` process triggered by a chained `next_action`, a queued
message, or an explicit user/API follow-up.

### Observed incident (workspace `7030-is-there-a-way-t`)

1. Agent starts `onnx` install via background Bash, ends turn ("harness will notify").
2. vibe-kanban `break`s on `Result`, closes stdin, Claude exits, background install dies.
   Session sits idle indefinitely. **(Problem 1 — the feature gap.)**
3. User follow-ups spawn fresh `claude --resume` processes that crash at the `Stop`
   git-check hook with `Stream closed at sendRequest` — Claude tries to round-trip the
   Stop hook over a stream vibe-kanban has already torn down. Session is unrecoverable.
   **(Problem 2 — a teardown race, hypothesized to be a consequence of the abrupt kill.)**

## Goals

- After the Claude agent starts a background shell task and ends its turn, the **same live
  session** continues automatically when the task completes, with full context preserved.
- No new tools and no injected agent instructions — rely on Claude's native
  "background task → harness will notify" behavior.
- Follow-ups never break: a session with a pending or completed background task remains
  recoverable, and manual stop always works.

## Non-goals (v1)

- Background **subagents** (`Task` tool). They have a separate, cleaner signal
  (`task_started` / `task_notification`, already parsed) and can be a follow-up.
- Plain detached `cmd &` in a foreground Bash call (invisible to Claude; no notify).
- Other executors (Codex, Gemini, …).
- A max-wait timeout. **Wait is unbounded**; the user stops the session manually if needed.

## Approach

Keep the persistent SDK session alive while background tasks are outstanding, and let
Claude's own harness perform the waiting, wakeup, and continuation. vibe-kanban's only job
is to (a) not tear the session down too early, (b) know when it is finally safe to tear it
down, and (c) tear down cleanly so `--resume` stays healthy.

### Tracking outstanding background tasks

Confirmed by the spike (see "## Spike findings"): background bash surfaces through the same
`system`/`task_*` messages Claude uses for async work, keyed by a `task_id`. Track a set of
outstanding `task_id`s from the stream:

- **Start:** a `system` message with `subtype: "task_started"` and a `task_id` → insert the
  `task_id` into the outstanding set.
- **Resolved:** a `system` message with `subtype: "task_notification"` and the same
  `task_id` (any terminal `status`, e.g. `completed` / `failed` / `stopped`) → remove it.
- `subtype: "task_updated"` is progress only — ignore for counting.

This is keyed by `task_id`, so it handles multiple concurrent background tasks correctly and
is self-correcting: a foreground task that starts and notifies within a single turn nets to
empty before its `Result`, so it never extends the session. It also needs no `Bash`-tool
field parsing and covers background subagents for free.

### Turn lifecycle change (read loop, `protocol.rs`)

- On `Result` with an **empty** outstanding set → finalize as today: stop reading, close
  stdin, let Claude exit, run commit / next-action / finalization.
- On `Result` with a **non-empty** set → **do not break**. Keep stdin open, keep reading.
  The execution process stays `Running`. Claude's harness fires `task_notification` on
  completion and **auto-continues on its own** (spike-confirmed — no nudge needed), emitting
  a final `Result` with an empty set — only then finalize.

This moves "turn is over" from "first `Result`" to "`Result` with no outstanding
background work," while keeping everything inside one live session (no `--resume`, no
context loss).

### Interaction with the completion watcher (`container.rs`)

`spawn_os_exit_watcher` waits for the whole process group to exit. Because we now keep the
Claude leader alive across the background task, the group naturally stays alive too; the
execution is correctly still `Running`. When the final `Result` arrives and the read loop
closes stdin, Claude exits, the background child is already gone, the group drains, and the
existing completion path runs unchanged.

### Graceful teardown (Problem 2)

Fix the teardown race so a session with hook round-trips in flight is never left in a
broken state:

- Before closing stdin on final teardown, allow pending hook control round-trips
  (e.g. the `Stop` git-check callback) to settle rather than dropping the stream mid-handshake.
- Ensure a completed or manually-stopped session leaves persisted state that a subsequent
  `--resume` can recover cleanly.

Root cause of the `Stream closed at sendRequest` crash will be pinned during
implementation via systematic-debugging; the hypothesis is that keeping the session alive
and tearing down only on the final `Result` removes the abrupt-kill condition that triggers
it. If a residual race remains, it is fixed here.

### User-facing state

- The workspace/execution visibly indicates it is **waiting on a background task** (distinct
  from an active agent turn), so an idle-looking-but-alive session is not mistaken for a hang.
- Manual **stop** tears the session down cleanly (kills the group, marks the execution
  killed) at any point, including while waiting.

## Components touched

| Concern | Location |
|---|---|
| Read-loop turn-end decision (don't break on non-final `Result`) | `crates/executors/src/executors/claude/protocol.rs:49-105` |
| Detect `run_in_background` Bash start / completion; outstanding counter | `crates/executors/src/executors/claude.rs` (`ClaudeLogProcessor`, tool parsing) |
| Completion watcher / process-group reconciliation | `crates/local-deployment/src/container.rs:841-899` |
| `Stop` hook round-trip vs. teardown ordering | `crates/executors/src/executors/claude/protocol.rs`, `claude/client.rs:323-347` |
| "Waiting on background task" execution state (UI + types) | execution-process status surface + `shared/types.ts` regen |

## Spike findings (resolved 2026-07-13, `claude` 2.1.198)

Driven headless exactly as the executor spawns it (`-p --output-format=stream-json
--input-format=stream-json --include-partial-messages --replay-user-messages`), stdin held
open, with a `run_in_background` Bash (`sleep 20 && echo BGDONE`). Canonical stream captured
in `crates/executors/src/executors/claude/fixtures/bg_bash_stream.jsonl`. Observed order:

```
assistant  tool_use Bash {command, description, run_in_background:true}
system     subtype=task_started      task_id=bl37ls4es
result     subtype=success                                 # turn 1 ends here — today we WRONGLY break
system     subtype=task_updated      task_id=bl37ls4es     # progress (ignore)
system     subtype=task_notification task_id=bl37ls4es status=completed
                                       summary="Background command ... completed (exit code 0)"
result     subtype=success                                 # turn 2 — the true final result
```

1. **Auto-continue: YES.** With stdin held open, after the first `result` the session parked,
   then at ~20s the `task_notification` arrived and the model **resumed on its own** (read the
   task output, summarized, emitted the final `result`) — no injected user message. So no
   nudge is needed; keeping stdin open is sufficient.
2. **Start signal:** `system` / `subtype:"task_started"` carrying `task_id` (and
   `tool_use_id`). **Completion signal:** `system` / `subtype:"task_notification"` carrying
   the same `task_id` and a terminal `status`. This is why tracking is keyed by `task_id`
   rather than by parsing the `Bash` tool.
3. **Teardown race:** closing stdin ~1s after the first `result` (background still running)
   makes Claude **stop** the task (`task_notification status:"stopped"`) and exit cleanly
   (code 0) — i.e. the premature close is exactly what kills the task today. The
   `Stream closed at sendRequest` crash is specific to the registered **`Stop` git-check
   hook** control round-trip (not reproduced by the plain driver, which registers no hooks);
   it is downstream of the same premature-teardown condition. Task 6 pins and guards it.

## Testing strategy

- **Unit (`ClaudeLogProcessor`):** background-Bash tool_use increments the outstanding
  counter; the completion signal decrements it; a `Result` with a non-zero counter is not
  treated as final.
- **Protocol read loop:** a synthetic stream `Result(pending) → <bg completion> → Result(none)`
  finalizes only on the second `Result`; `Result(none)` alone finalizes immediately
  (regression guard for the common no-background case).
- **Teardown:** a session with a pending `Stop`-hook round-trip tears down without a
  `Stream closed` error and remains `--resume`-able.
- **Manual e2e (from the spike harness):** real background command → verify the same session
  continues and commits after completion; verify manual stop mid-wait.
