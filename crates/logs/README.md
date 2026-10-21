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

## Config example

```yaml
# ~/.devlanes/config.yaml: service output kept on disk, by age and total size.
logs:
  retention: { days: 7, max_size: 5GiB }
```

The daemon passes these bounds to every unit's `LogOptions`. Its maintenance pass
applies the size bound across all unit directories.

## API

- `UnitLog::open`: the single writer of a unit directory; `Error::Locked` while another writer holds it.
- `UnitLog::reader`: a metadata snapshot to query after releasing any lock around the writer.
- `LogReader::open`: reads a directory without recovering, locking or creating anything.
- `UnitLog::trim_to(max_bytes) -> Result<u64>`: removes oldest data until the directory fits `max_bytes`; returns the bytes left.
- `UnitLog::disk_bytes`: bytes the directory's segments use now.

## Behaviour

The API blocks on disk I/O and compression. Use a dedicated thread or
`spawn_blocking`. A `.writer.lock` file keeps a second writer out of a directory
until `finish` or drop.

Segments rotate by size or age; closed segments use zstd compression by default and
keep their index in a `.meta.json` file, so opening reads only those files and the
previous run's unfinished segment. A segment that fails to load is renamed to
`*.corrupt` with a warning. `segments()` exposes metadata for the daemon to persist.
Carry `next_sequence()` into the next run's `LogOptions::initial_sequence` to
preserve cursor identity.

Queries support grep, severity filters, time windows and context around a cursor.
Forward cursors use `unit#seq`; tail pages return `before:unit#seq` to fetch older
records. Pass the returned `next_cursor` back as `LogQuery::cursor`. Tail queries
read the newest segments first and stop once the page is full.

## License

Apache-2.0
