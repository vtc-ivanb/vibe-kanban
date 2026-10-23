-- Cached AI-generated summaries of a workspace's work, shown in the activity report.
-- `source_activity_at` is the latest activity the summary was generated from; a
-- summary is stale once the workspace has activity after that timestamp.
CREATE TABLE workspace_report_summaries (
    workspace_id       BLOB PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    summary            TEXT NOT NULL,
    source_activity_at TEXT NOT NULL,
    executor_profile   TEXT NOT NULL,
    created_at         TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at         TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
);
