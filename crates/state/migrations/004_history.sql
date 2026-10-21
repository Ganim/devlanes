CREATE TABLE log_segments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    unit_run_id INTEGER NOT NULL REFERENCES unit_runs(id) ON DELETE CASCADE,
    first_seq INTEGER NOT NULL,
    last_seq INTEGER NOT NULL,
    min_ts INTEGER NOT NULL,
    max_ts INTEGER NOT NULL,
    path BLOB NOT NULL,
    compressed INTEGER NOT NULL CHECK(compressed IN (0, 1)),
    bytes INTEGER NOT NULL CHECK(bytes >= 0),
    UNIQUE(unit_run_id, path)
);
CREATE TABLE metric_rollups (
    series TEXT NOT NULL,
    minute INTEGER NOT NULL,
    samples INTEGER NOT NULL,
    cpu_min REAL NOT NULL,
    cpu_avg REAL NOT NULL,
    cpu_max REAL NOT NULL,
    mem_min INTEGER NOT NULL,
    mem_avg INTEGER NOT NULL,
    mem_max INTEGER NOT NULL,
    rss_max INTEGER NOT NULL,
    PRIMARY KEY(series, minute)
);
CREATE INDEX metric_rollups_minute ON metric_rollups(minute);
CREATE TABLE run_summaries (
    unit_run_id INTEGER PRIMARY KEY REFERENCES unit_runs(id) ON DELETE CASCADE,
    series TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER NOT NULL,
    samples INTEGER NOT NULL,
    mem_p50 INTEGER NOT NULL,
    mem_p95 INTEGER NOT NULL,
    mem_peak INTEGER NOT NULL,
    cpu_p50 REAL NOT NULL,
    cpu_p95 REAL NOT NULL,
    cpu_peak REAL NOT NULL,
    rss_peak INTEGER NOT NULL
);
CREATE INDEX run_summaries_series ON run_summaries(series, ended_at);
