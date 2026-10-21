-- Rows from an earlier boot keep NULL: their PIDs are probed as before.
ALTER TABLE unit_runs ADD COLUMN boot_id TEXT;
-- Descendants that left the leader's session, so stopping an orphan after a
-- daemon crash can still reach them through the same identity checks.
CREATE TABLE unit_run_members (
    unit_run_id BIGINT NOT NULL REFERENCES unit_runs(id) ON DELETE CASCADE,
    pid BIGINT NOT NULL CHECK(pid > 0),
    sid BIGINT NOT NULL CHECK(sid >= 0),
    process_start_time TEXT NOT NULL,
    PRIMARY KEY(unit_run_id, pid, process_start_time)
);
