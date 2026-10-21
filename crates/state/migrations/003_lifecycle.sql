ALTER TABLE projects ADD COLUMN config_dir BLOB;
CREATE TABLE lane_setup (
    lane_id INTEGER NOT NULL REFERENCES lanes(id) ON DELETE CASCADE,
    entry TEXT NOT NULL CHECK(entry != ''),
    done_at INTEGER NOT NULL,
    container_started_at TEXT,
    PRIMARY KEY(lane_id, entry)
);
CREATE TABLE task_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    lane_id INTEGER NOT NULL REFERENCES lanes(id) ON DELETE CASCADE,
    task TEXT NOT NULL CHECK(task != ''),
    trigger TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    exit_code INTEGER,
    actor_kind TEXT NOT NULL CHECK(actor_kind IN ('human', 'agent')),
    actor_agent_id TEXT,
    actor_session_id TEXT
);
CREATE INDEX task_runs_lane_task ON task_runs(lane_id, task);
CREATE INDEX task_runs_ended ON task_runs(ended_at);
CREATE INDEX unit_runs_ended ON unit_runs(ended_at);
