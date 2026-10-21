CREATE TABLE log_segments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    unit_run_id BIGINT NOT NULL REFERENCES unit_runs(id) ON DELETE CASCADE,
    first_seq BIGINT NOT NULL,
    last_seq BIGINT NOT NULL,
    min_ts BIGINT NOT NULL,
    max_ts BIGINT NOT NULL,
    path BLOB NOT NULL,
    compressed INTEGER NOT NULL CHECK(compressed IN (0, 1)),
    bytes BIGINT NOT NULL CHECK(bytes >= 0),
    UNIQUE(unit_run_id, path)
);
CREATE TABLE metric_rollups (
    series TEXT NOT NULL,
    minute BIGINT NOT NULL,
    samples BIGINT NOT NULL,
    cpu_min DOUBLE NOT NULL,
    cpu_avg DOUBLE NOT NULL,
    cpu_max DOUBLE NOT NULL,
    mem_min BIGINT NOT NULL,
    mem_avg BIGINT NOT NULL,
    mem_max BIGINT NOT NULL,
    rss_max BIGINT NOT NULL,
    PRIMARY KEY(series, minute)
);
CREATE INDEX metric_rollups_minute ON metric_rollups(minute);
CREATE TABLE run_summaries (
    unit_run_id INTEGER PRIMARY KEY REFERENCES unit_runs(id) ON DELETE CASCADE,
    series TEXT NOT NULL,
    started_at BIGINT NOT NULL,
    ended_at BIGINT NOT NULL,
    samples BIGINT NOT NULL,
    mem_p50 BIGINT NOT NULL,
    mem_p95 BIGINT NOT NULL,
    mem_peak BIGINT NOT NULL,
    cpu_p50 DOUBLE NOT NULL,
    cpu_p95 DOUBLE NOT NULL,
    cpu_peak DOUBLE NOT NULL,
    rss_peak BIGINT NOT NULL
);
CREATE INDEX run_summaries_series ON run_summaries(series, ended_at);
CREATE INDEX run_summaries_ended ON run_summaries(ended_at);
CREATE INDEX log_segments_max_ts ON log_segments(max_ts);
