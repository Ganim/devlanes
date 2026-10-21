# devlanes-logs

The daemon's per-unit log store, with a separate directory for each run.
It frames process output into searchable records and returns `devlanes-proto` query results; the daemon owns database persistence.

## Usage

```rust
use devlanes_logs::{LogOptions, UnitLog};
use devlanes_proto::LogQuery;

fn main() -> devlanes_logs::Result<()> {
    let mut logs = UnitLog::open("logs/api/run-0001", "api", LogOptions::default())?;
    logs.ingest(b"ERROR connection refused\n", 1_000)?;

    let page = logs.query(&LogQuery {
        tail: Some(100),
        max_bytes: Some(16_384),
        ..LogQuery::default()
    })?;
    for line in page.items {
        println!("{}#{} {}", line.unit, line.seq, line.text);
    }

    logs.finish(1_001)?;
    Ok(())
}
```

Timestamps are receive times in Unix milliseconds. During capture, call
`flush_partial(now_ms, false)` every 200 ms to expose incomplete lines.
Use `ingest_stream` for separate stdout and stderr streams.

## Storage and queries

The API blocks on disk I/O and compression. Use a dedicated thread or
`spawn_blocking`, with one writer per run directory.

Segments rotate by size or age; closed segments use zstd compression by default.
`segments()` exposes metadata for the daemon to persist. Carry `next_sequence()`
into the next run's `LogOptions::initial_sequence` to preserve cursor identity.

Queries support grep, severity filters, time windows and context around a cursor.
Forward cursors use `unit#seq`; tail pages return `before:unit#seq` to fetch older
records. Pass the returned `next_cursor` back as `LogQuery::cursor`.

## License

Apache-2.0
