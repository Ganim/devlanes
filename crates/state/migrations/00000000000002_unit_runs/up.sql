CREATE TABLE unit_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    lane_id BIGINT NOT NULL REFERENCES lanes(id) ON DELETE CASCADE,
    unit TEXT NOT NULL,
    pid BIGINT NOT NULL CHECK(pid > 0),
    sid BIGINT NOT NULL CHECK(sid > 0),
    process_start_time TEXT NOT NULL,
    started_at BIGINT NOT NULL,
    ended_at BIGINT,
    exit_code INTEGER,
    exit_signal INTEGER,
    actor_kind TEXT NOT NULL CHECK(actor_kind IN ('human', 'agent')),
    actor_agent_id TEXT,
    actor_session_id TEXT,
    log_path BLOB
);
CREATE UNIQUE INDEX unit_runs_active ON unit_runs(lane_id, unit) WHERE ended_at IS NULL;
CREATE INDEX unit_runs_actor ON unit_runs(actor_session_id);
