-- Peak tree memory of a unit while it started (until ready or settled), for
-- the start gate's memory check. A handful of recent rows per series.
CREATE TABLE startup_peaks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    series TEXT NOT NULL CHECK(series != ''),
    recorded_at BIGINT NOT NULL,
    peak BIGINT NOT NULL CHECK(peak >= 0),
    duration_ms BIGINT NOT NULL CHECK(duration_ms >= 0)
);
CREATE INDEX startup_peaks_series ON startup_peaks(series, recorded_at);
