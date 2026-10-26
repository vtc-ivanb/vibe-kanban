-- Report summaries describe the work done in a specific report period, so they
-- are cached per (workspace, period). Earlier rows summarized whole workspace
-- histories and are not valid for any period, so they are dropped.
DROP TABLE IF EXISTS workspace_report_summaries;

CREATE TABLE workspace_report_summaries (
    workspace_id       BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- Report period as Unix seconds: [period_from, period_to).
    period_from        INTEGER NOT NULL,
    period_to          INTEGER NOT NULL,
    -- A few words naming the work done in the period, e.g. "Fixing pnpm tests".
    title              TEXT NOT NULL,
    summary            TEXT NOT NULL,
    -- Latest agent activity within the period the summary was generated from;
    -- the summary is stale once the period has newer activity.
    source_activity_at TEXT NOT NULL,
    executor_profile   TEXT NOT NULL,
    created_at         TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at         TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    PRIMARY KEY (workspace_id, period_from, period_to)
);
