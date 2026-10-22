#![allow(clippy::unwrap_used)]

use crate::{Error, Frame, Framer, LogOptions, LogReader, MAX_LINE_BYTES, UnitLog, extract_level};
use devlanes_proto::LogQuery;
use std::{fs, io::Write, path::Path};

/// Small segments exercise real compression and indexes without large fixtures.
fn store(options: LogOptions) -> (tempfile::TempDir, UnitLog) {
    let directory = tempfile::tempdir().unwrap();
    let log = UnitLog::open(directory.path(), "api", options).unwrap();
    (directory, log)
}

/// Collect only in tests; production callers use the bounded callback interface.
fn feed(framer: &mut Framer, bytes: &[u8], timestamp: u64, frames: &mut Vec<Frame>) {
    framer
        .ingest(bytes, timestamp, |frame| {
            frames.push(frame);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
/// Every byte boundary may split ANSI, OSC hyperlinks, CRLF, or UTF-8.
fn arbitrary_chunks_strip_ansi_and_replace_progress() {
    let input = b"old\rnew\x1b[31m\xe2\x82\xac\x1b[0m\r\n\x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\\n";
    for split in 0..=input.len() {
        let mut framer = Framer::default();
        let mut frames = Vec::new();
        feed(&mut framer, &input[..split], 1, &mut frames);
        feed(&mut framer, &input[split..], 2, &mut frames);
        assert_eq!(
            frames.iter().map(|f| f.text.as_str()).collect::<Vec<_>>(),
            ["new€", "link"]
        );
        assert_eq!(
            frames
                .iter()
                .flat_map(|f| f.raw.iter().copied())
                .collect::<Vec<_>>(),
            input
        );
    }
}

#[test]
/// Timer flushing uses idle time and makes EOF's incomplete UTF-8 visible.
fn partial_flush_and_invalid_utf8() {
    let mut framer = Framer::default();
    let mut frames = Vec::new();
    feed(&mut framer, b"prompt?", 10, &mut frames);
    assert!(framer.flush_partial(209, false).is_none());
    let partial = framer.flush_partial(210, false).unwrap();
    assert!(partial.partial);
    assert_eq!(partial.timestamp_ms, 10);
    assert_eq!(partial.text, "prompt?");
    feed(&mut framer, b"\n\ninvalid \xff\xe2", 300, &mut frames);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].text, "");
    assert_eq!(framer.flush_partial(300, true).unwrap().text, "invalid ��");
    feed(&mut framer, b"\xe2", 400, &mut frames);
    assert_eq!(framer.flush_partial(600, false).unwrap().text, "");
    assert_eq!(framer.flush_partial(600, true).unwrap().text, "�");
}

#[test]
/// Huge unterminated hyperlinks cannot accumulate inside the terminal parser.
fn osc_across_line_caps_does_not_leak_into_searchable_text() {
    let mut framer = Framer::default();
    let mut frames = Vec::new();
    feed(&mut framer, b"\x1b]8;;", 1, &mut frames);
    feed(&mut framer, &vec![b'a'; MAX_LINE_BYTES * 4], 1, &mut frames);
    feed(&mut framer, b"\x1b\\visible\n", 1, &mut frames);
    assert_eq!(
        frames.iter().map(|f| f.text.as_str()).collect::<String>(),
        "visible"
    );
    assert!(frames.iter().all(|f| f.raw.len() <= MAX_LINE_BYTES));
}

#[test]
/// A code point spanning the raw cap belongs wholly to the next text fragment.
fn long_lines_cap_without_corrupting_unicode_or_adding_empty_lines() {
    let mut framer = Framer::default();
    let mut frames = Vec::new();
    let text = format!("{}€\n", "a".repeat(MAX_LINE_BYTES - 1));
    feed(&mut framer, text.as_bytes(), 1, &mut frames);
    assert_eq!(frames.len(), 2);
    assert!(frames[0].continued);
    assert_eq!(frames[0].raw.len(), MAX_LINE_BYTES);
    assert_eq!(frames[1].text, "€");
    assert_eq!(
        frames.iter().map(|f| f.text.as_str()).collect::<String>(),
        text.trim_end()
    );
    frames.clear();
    feed(
        &mut framer,
        format!("{}\n", "b".repeat(MAX_LINE_BYTES)).as_bytes(),
        2,
        &mut frames,
    );
    assert_eq!(frames.len(), 1);
}

#[test]
/// Structured severity beats message content and supports pino and systemd encodings.
fn extracts_structured_levels() {
    for (text, expected) in [
        (r#"{"level":50,"msg":"info"}"#, Some("error")),
        (r#"[worker] {"severity":"WARNING"}"#, Some("warn")),
        (r#"{"log":{"level":"DEBUG"}}"#, Some("debug")),
        (r#"{"PRIORITY":3}"#, Some("error")),
        (r#"msg="a level=error b" level="info""#, Some("info")),
        (r#"msg="level=error""#, None),
        ("WARNING: server stopped", Some("warn")),
        ("30 requests completed", None),
        (r#"{"msg":"error"}"#, None),
    ] {
        assert_eq!(extract_level(text).as_deref(), expected, "{text}");
    }
}

#[test]
/// Queries read compressed history and the active buffered segment without duplicates.
fn rotation_compression_grep_since_levels_and_context() {
    let options = LogOptions {
        segment_bytes: 90,
        ..LogOptions::default()
    };
    let (_directory, mut log) = store(options);
    for i in 1..=8 {
        let text = if i == 4 {
            "ERROR needle"
        } else {
            "INFO normal"
        };
        log.ingest(format!("{text} {i}\n").as_bytes(), i * 1000)
            .unwrap();
    }
    assert!(log.segments().iter().any(|m| m.compressed));
    assert!(
        log.segments()
            .iter()
            .filter(|m| m.closed)
            .all(|m| m.path.extension().unwrap() == "zst")
    );
    let query = LogQuery {
        grep: Some("nee.*le".into()),
        context_lines: 1,
        ..Default::default()
    };
    let page = log.query(&query).unwrap();
    assert_eq!(
        page.items.iter().map(|l| l.seq).collect::<Vec<_>>(),
        [3, 4, 5]
    );
    let query = LogQuery {
        since: Some("3s".into()),
        level: Some("info".into()),
        ..Default::default()
    };
    assert_eq!(
        log.query_at(&query, 8000)
            .unwrap()
            .items
            .iter()
            .map(|l| l.seq)
            .collect::<Vec<_>>(),
        [5, 6, 7, 8]
    );
    let query = LogQuery {
        since: Some("1970-01-01T00:00:04Z".into()),
        level: Some("warn".into()),
        ..Default::default()
    };
    assert_eq!(log.query(&query).unwrap().items[0].seq, 4);
    assert!(matches!(
        log.query(&LogQuery {
            grep: Some("[".into()),
            ..Default::default()
        }),
        Err(Error::Query(_))
    ));
}

#[test]
/// Tail continuation walks backwards while forward continuation excludes its boundary.
fn cursors_tail_around_and_byte_budgets() {
    let (_directory, mut log) = store(LogOptions::default());
    for i in 1..=12 {
        log.ingest(format!("line {i}\n").as_bytes(), i).unwrap();
    }
    let mut query = LogQuery {
        tail: Some(3),
        ..Default::default()
    };
    let page = log.query(&query).unwrap();
    assert_eq!(
        page.items.iter().map(|l| l.seq).collect::<Vec<_>>(),
        [10, 11, 12]
    );
    assert!(page.truncated);
    assert_eq!(page.next_cursor.as_deref(), Some("before:api#10"));
    query.cursor = page.next_cursor;
    assert_eq!(
        log.query(&query)
            .unwrap()
            .items
            .iter()
            .map(|l| l.seq)
            .collect::<Vec<_>>(),
        [7, 8, 9]
    );
    let around = LogQuery {
        around: Some("api#5".into()),
        context_lines: 2,
        ..Default::default()
    };
    assert_eq!(
        log.query(&around)
            .unwrap()
            .items
            .iter()
            .map(|l| l.seq)
            .collect::<Vec<_>>(),
        [3, 4, 5, 6, 7]
    );
    let mut query = LogQuery {
        max_bytes: Some(200),
        ..Default::default()
    };
    let mut seen = Vec::new();
    loop {
        let page = log.query(&query).unwrap();
        assert!(
            page.items
                .iter()
                .map(|l| serde_json::to_vec(l).unwrap().len() + 1)
                .sum::<usize>()
                <= 200
        );
        seen.extend(page.items.into_iter().map(|l| l.seq));
        query.cursor = page.next_cursor;
        if query.cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, (1..=12).collect::<Vec<_>>());
    assert!(
        log.query(&LogQuery {
            cursor: Some("other#3".into()),
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
/// Budget accounting includes UTF-8 and JSON escapes and still advances pagination.
fn truncation_is_utf8_safe_and_wire_byte_bounded() {
    let (_directory, mut log) = store(LogOptions::default());
    log.ingest(format!("{}\nnext\n", "€\t\"".repeat(200)).as_bytes(), 100)
        .unwrap();
    let mut query = LogQuery {
        max_bytes: Some(170),
        ..Default::default()
    };
    let page = log.query(&query).unwrap();
    assert!(page.truncated);
    assert_eq!(page.items.len(), 1);
    assert!(page.items[0].text.len() < 1000);
    assert!(serde_json::to_vec(&page.items[0]).unwrap().len() < 170);
    query.cursor = page.next_cursor;
    assert_eq!(log.query(&query).unwrap().items[0].text, "next");
    assert!(
        log.query(&LogQuery {
            max_bytes: Some(1),
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
/// Idle rotation and retention preserve next sequence even when all history expires.
fn age_rotation_retention_and_reopen() {
    let options = LogOptions {
        segment_age_ms: 50,
        retention_age_ms: 100,
        retention_bytes: u64::MAX,
        ..Default::default()
    };
    let (directory, mut log) = store(options.clone());
    log.ingest(b"old\n", 10).unwrap();
    log.maintain(60).unwrap();
    assert!(log.segments()[0].compressed);
    log.maintain(111).unwrap();
    assert_eq!(log.segments().len(), 1);
    drop(log);
    let mut log = UnitLog::open(directory.path(), "api", options).unwrap();
    assert_eq!(log.next_sequence(), 2);
    log.ingest(b"new\n", 120).unwrap();
    assert_eq!(log.query(&LogQuery::default()).unwrap().items[0].seq, 2);
}

#[test]
/// Recovery truncates a torn tail before appending; compressed segments remain searchable.
fn reopening_recovers_torn_tail_and_indexes() {
    let options = LogOptions {
        segment_bytes: 50,
        ..Default::default()
    };
    let (directory, mut log) = store(options.clone());
    log.ingest(b"first\nsecond\nthird\n", 1).unwrap();
    log.flush().unwrap();
    let active = log.segments().last().unwrap().path.clone();
    drop(log);
    fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .unwrap()
        .write_all(b"4\t2\t")
        .unwrap();
    let mut log = UnitLog::open(directory.path(), "api", options).unwrap();
    log.ingest(b"fourth\n", 2).unwrap();
    assert_eq!(
        log.query(&LogQuery::default())
            .unwrap()
            .items
            .iter()
            .map(|l| l.seq)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
}

#[test]
/// Stream separation prevents a stderr line from swallowing a pending stdout prompt.
fn independent_streams_and_sparse_index() {
    let (_directory, mut log) = store(LogOptions::default());
    log.ingest_stream("out", b"prompt", 1).unwrap();
    log.ingest_stream("err", b"warning\n", 2).unwrap();
    log.ingest_stream("out", b" answer\n", 3).unwrap();
    for _ in 0..600 {
        log.ingest(b"other\n", 4).unwrap();
    }
    assert_eq!(log.segments()[0].index.len(), 3);
    let page = log
        .query(&LogQuery {
            cursor: Some("api#599".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        page.items.iter().map(|l| l.seq).collect::<Vec<_>>(),
        [600, 601, 602]
    );
    let page = log.query(&LogQuery::default()).unwrap();
    assert_eq!(page.items[0].text, "warning");
    assert_eq!(page.items[1].text, "prompt answer");
}

#[test]
/// Byte retention removes closed files and leaves active data queryable.
fn size_retention_removes_metadata_and_compressed_files() {
    let options = LogOptions {
        segment_bytes: 30,
        retention_bytes: 1,
        ..Default::default()
    };
    let (directory, mut log) = store(options);
    log.ingest(b"one long line\ntwo long line\nthree long line\n", 1)
        .unwrap();
    assert_eq!(log.segments().len(), 1);
    assert_eq!(listing(directory.path()), ["00000000000000000003.log"]);
    assert_eq!(log.query(&LogQuery::default()).unwrap().items[0].seq, 3);
}

#[test]
/// Recovery removes a stale compression destination before it can resurrect retained data.
fn interrupted_compression_uses_plain_source_and_rebuilds_metadata() {
    let options = LogOptions {
        compress: false,
        ..Default::default()
    };
    let (directory, mut log) = store(options.clone());
    log.ingest(b"source\n", 1).unwrap();
    log.flush().unwrap();
    let path = log.segments()[0].path.clone();
    drop(log);
    let stale = path.with_extension("log.zst");
    fs::write(
        &stale,
        zstd::stream::encode_all(&b"stale and invalid"[..], 3).unwrap(),
    )
    .unwrap();
    let mut log = UnitLog::open(directory.path(), "api", options).unwrap();
    assert!(!stale.exists());
    assert_eq!(
        log.query(&LogQuery::default()).unwrap().items[0].text,
        "source"
    );
    assert!(
        directory
            .path()
            .join("00000000000000000001.meta.json")
            .exists()
    );
}

#[test]
/// A damaged closed segment is moved aside so the unit can still start and log.
fn corrupt_closed_segment_is_quarantined() {
    let options = LogOptions {
        compress: false,
        ..Default::default()
    };
    let (directory, mut log) = store(options.clone());
    log.ingest(b"closed\n", 1).unwrap();
    log.finish(2).unwrap();
    let path = log.segments()[0].path.clone();
    drop(log);
    fs::write(&path, b"1\t1\t0\t-\tunterminated").unwrap();
    let mut log = UnitLog::open(directory.path(), "api", options).unwrap();
    assert!(!path.exists());
    assert_eq!(
        listing(directory.path()),
        [
            "00000000000000000001.log.corrupt",
            "00000000000000000001.meta.json.corrupt",
            "00000000000000000002.log",
        ]
    );
    log.ingest(b"after\n", 3).unwrap();
    let page = log.query(&LogQuery::default()).unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|l| (l.seq, l.text.as_str()))
            .collect::<Vec<_>>(),
        [(2, "after")]
    );
}

#[test]
/// A torn interior record in the previous run's tail quarantines only that segment.
fn corrupt_tail_is_quarantined_and_history_kept() {
    let options = LogOptions {
        segment_bytes: 30,
        ..Default::default()
    };
    let (directory, mut log) = store(options.clone());
    log.ingest(b"one long line\ntwo long line\n", 1).unwrap();
    log.flush().unwrap();
    let tail = log.segments().last().unwrap().path.clone();
    drop(log);
    fs::write(&tail, b"2\t1\t0\t-\tok\nbroken\n").unwrap();
    let mut log = UnitLog::open(directory.path(), "api", options).unwrap();
    assert!(Path::new(&format!("{}.corrupt", tail.display())).exists());
    log.ingest(b"three\n", 2).unwrap();
    let page = log.query(&LogQuery::default()).unwrap();
    assert_eq!(page.items.iter().map(|l| l.seq).collect::<Vec<_>>(), [1, 3]);
}

#[test]
/// A backwards wall-clock adjustment must not prune later sequence numbers.
fn since_pruning_uses_timestamp_extrema_and_cross_run_sequence_seed() {
    let options = LogOptions {
        initial_sequence: 100,
        ..Default::default()
    };
    let (_directory, mut log) = store(options);
    log.ingest(b"new clock\n", 1000).unwrap();
    log.ingest(b"old clock\n", 100).unwrap();
    log.finish(1000).unwrap();
    let page = log
        .query(&LogQuery {
            since: Some("500".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].seq, 100);
    assert_eq!(log.next_sequence(), 102);
    assert_eq!(log.segments()[0].min_timestamp_ms, 100);
    assert_eq!(log.segments()[0].max_timestamp_ms, 1000);
}

/// Visible directory entries, sorted; the writer's lock file is hidden.
fn listing(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Names and contents, to prove a read left the directory untouched.
fn contents(directory: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().into_string().unwrap(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    entries.sort();
    entries
}

/// Several compressed segments, each spanning more than one 256-record index block.
fn history(directory: &Path, lines: u64) -> UnitLog {
    let options = LogOptions {
        segment_bytes: 16 * 1024,
        ..Default::default()
    };
    let mut log = UnitLog::open(directory, "api", options).unwrap();
    for i in 1..=lines {
        log.ingest(format!("line {i}\n").as_bytes(), i).unwrap();
    }
    assert!(log.segments().iter().filter(|m| m.compressed).count() >= 3);
    assert!(log.segments()[0].index.len() >= 2);
    log
}

fn seqs(log: &mut UnitLog, query: &LogQuery) -> Vec<u64> {
    log.query(query)
        .unwrap()
        .items
        .iter()
        .map(|l| l.seq)
        .collect()
}

#[test]
/// Opening trusts closed segments' metadata instead of decompressing them, and
/// removes temporaries an interrupted compression or metadata write left behind.
fn open_loads_closed_metadata_without_decoding() {
    let directory = tempfile::tempdir().unwrap();
    let log = history(directory.path(), 3000);
    let closed = log.segments()[0].clone();
    let rebuilt = directory
        .path()
        .join(format!("{:020}.meta.json", log.segments()[1].first_seq));
    drop(log);
    // Same length, so the metadata stays valid; decoding would fail.
    fs::write(&closed.path, vec![0; closed.bytes as usize]).unwrap();
    // A crash between compression and the metadata write costs one scan.
    fs::remove_file(&rebuilt).unwrap();
    let zst_tmp = directory.path().join("00000000000000000001.log.zst.tmp");
    let json_tmp = directory.path().join("00000000000000000001.meta.json.tmp");
    fs::write(&zst_tmp, b"partial").unwrap();
    fs::write(&json_tmp, b"{").unwrap();
    let log = UnitLog::open(directory.path(), "api", LogOptions::default()).unwrap();
    assert_eq!(log.segments()[0].path, closed.path);
    assert_eq!(log.next_sequence(), 3001);
    assert!(rebuilt.exists());
    assert!(!zst_tmp.exists() && !json_tmp.exists());
}

#[test]
/// Tail pages walk newest first and stop when full; continuation covers everything
/// once, and grep context matches the forward scan.
fn tail_walks_newest_first_across_compressed_blocks() {
    let directory = tempfile::tempdir().unwrap();
    let mut log = history(directory.path(), 3000);
    let mut query = LogQuery {
        tail: Some(100),
        ..Default::default()
    };
    let mut seen = Vec::new();
    loop {
        let page = log.query(&query).unwrap();
        assert!(page.items.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
        seen.splice(0..0, page.items.iter().map(|l| l.seq));
        query.cursor = page.next_cursor;
        if query.cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, (1..=3000).collect::<Vec<_>>());

    let grep = LogQuery {
        grep: Some("line (7|1500|2999)$".into()),
        context_lines: 2,
        ..Default::default()
    };
    let forward = seqs(&mut log, &grep);
    assert_eq!(
        forward,
        [
            5, 6, 7, 8, 9, 1498, 1499, 1500, 1501, 1502, 2997, 2998, 2999, 3000
        ]
    );
    let tail = LogQuery {
        tail: Some(1000),
        ..grep
    };
    assert_eq!(seqs(&mut log, &tail), forward);

    // The oldest segment is unreadable now; a tail never reaches it.
    let oldest = log.segments()[0].clone();
    fs::write(&oldest.path, vec![0; oldest.bytes as usize]).unwrap();
    let tail = LogQuery {
        tail: Some(10),
        ..Default::default()
    };
    assert_eq!(seqs(&mut log, &tail), (2991..=3000).collect::<Vec<_>>());
    assert!(log.query(&LogQuery::default()).is_err());
}

#[test]
/// A reader snapshot is independent of the writer: later writes stay invisible, a
/// rotated active segment is read from its compressed file, and removed segments
/// are skipped.
fn reader_snapshot_survives_writes_rotation_and_retention() {
    let (_directory, mut log) = store(LogOptions::default());
    log.ingest(b"one\n", 1).unwrap();
    let reader = log.reader();
    log.ingest(b"two\n", 2).unwrap();
    log.finish(3).unwrap();
    assert!(log.segments()[0].compressed);
    let page = reader.query(&LogQuery::default()).unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>(),
        ["one"]
    );

    let directory = tempfile::tempdir().unwrap();
    let mut log = history(directory.path(), 3000);
    let reader = log.reader();
    let segments = log.segments();
    fs::remove_file(&segments[0].path).unwrap();
    let page = reader.query(&LogQuery::default()).unwrap();
    assert_eq!(page.items[0].seq, segments[1].first_seq);
}

#[test]
/// One writer per directory; a read-only reader creates nothing, recovers nothing
/// and ignores a crashed writer's torn record.
fn writer_lock_and_read_only_reader() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let reader = LogReader::open(&missing, "api").unwrap();
    assert!(reader.query(&LogQuery::default()).unwrap().items.is_empty());
    assert!(!missing.exists());

    let run = directory.path().join("run");
    let options = LogOptions {
        segment_bytes: 30,
        ..Default::default()
    };
    let mut log = UnitLog::open(&run, "api", options.clone()).unwrap();
    assert!(matches!(
        UnitLog::open(&run, "api", options.clone()),
        Err(Error::Locked)
    ));
    log.ingest(b"one long line\ntwo long line\n", 1).unwrap();
    log.flush().unwrap();
    let tail = log.segments().last().unwrap().path.clone();
    drop(log);
    fs::OpenOptions::new()
        .append(true)
        .open(&tail)
        .unwrap()
        .write_all(b"3\t2\t")
        .unwrap();
    let before = contents(&run);
    let reader = LogReader::open(&run, "api").unwrap();
    assert_eq!(reader.segments().len(), 2);
    let page = reader.query(&LogQuery::default()).unwrap();
    assert_eq!(page.items.iter().map(|l| l.seq).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(contents(&run), before);

    // `finish` releases the lock while the value is still alive.
    let mut log = UnitLog::open(&run, "api", options.clone()).unwrap();
    log.finish(3).unwrap();
    let again = UnitLog::open(&run, "api", options).unwrap();
    assert_eq!(again.next_sequence(), 3);
}

#[test]
/// The idle tick makes buffered records visible to independent readers.
fn flush_partial_flushes_the_file_buffer() {
    let (directory, mut log) = store(LogOptions::default());
    log.ingest(b"buffered\n", 1).unwrap();
    let read = || {
        LogReader::open(directory.path(), "api")
            .unwrap()
            .query(&LogQuery::default())
            .unwrap()
            .items
            .len()
    };
    assert_eq!(read(), 0);
    log.flush_partial(2, false).unwrap();
    assert_eq!(read(), 1);
}

#[test]
/// A live unit's share of the global budget removes oldest data first, and a zero
/// budget closes the active segment so it can go too.
fn trim_to_enforces_a_byte_budget() {
    let directory = tempfile::tempdir().unwrap();
    let mut log = history(directory.path(), 3000);
    let total = log.disk_bytes();
    let oldest = log.segments()[0].path.clone();
    let left = log.trim_to(total / 2).unwrap();
    assert!(left <= total / 2);
    assert_eq!(left, log.disk_bytes());
    assert!(!oldest.exists());
    assert_eq!(log.trim_to(0).unwrap(), 0);
    assert_eq!(log.segments().len(), 1);
    assert_eq!(listing(directory.path()), ["00000000000000003001.log"]);
    log.ingest(b"after\n", 4000).unwrap();
    assert_eq!(seqs(&mut log, &LogQuery::default()), [3001]);
}

#[test]
/// SGR colors survive in `styled`, end in a reset, carry over to the next line
/// while they stay set, and other escapes never reach either form.
fn styled_lines_keep_only_colors() {
    let mut framer = Framer::default();
    let mut frames = Vec::new();
    let input = b"\x1b[32mINFO\x1b[0m ready \x1b[2K\x1b[1;34mblue\nstill blue\x1b[0m\nplain\n";
    for split in 0..=input.len() {
        frames.clear();
        let mut framer_split = Framer::default();
        feed(&mut framer_split, &input[..split], 1, &mut frames);
        feed(&mut framer_split, &input[split..], 2, &mut frames);
        let texts: Vec<&str> = frames.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, ["INFO ready blue", "still blue", "plain"]);
        let styled: Vec<Option<&str>> = frames.iter().map(|f| f.styled.as_deref()).collect();
        assert_eq!(
            styled,
            [
                Some("\x1b[32mINFO\x1b[0m ready \x1b[1;34mblue\x1b[0m"),
                Some("\x1b[1;34mstill blue\x1b[0m\x1b[0m"),
                None,
            ]
        );
    }
    feed(&mut framer, b"no color\n", 3, &mut frames);
    assert_eq!(frames.last().unwrap().styled, None);
}

#[test]
/// Stored colored lines read back with plain `text` for grep and levels, and
/// with `styled` only when the query asks for it.
fn styled_lines_round_trip_through_storage() {
    let (_directory, mut log) = store(LogOptions::default());
    log.ingest(b"\x1b[31mERROR\x1b[0m boom\nplain\n", 1)
        .unwrap();
    let plain = log.query(&LogQuery::default()).unwrap();
    assert_eq!(plain.items[0].text, "ERROR boom");
    assert_eq!(plain.items[0].level.as_deref(), Some("error"));
    assert!(plain.items.iter().all(|line| line.styled.is_none()));
    let colored = log
        .query(&LogQuery {
            styled: true,
            grep: Some("boom".into()),
            ..LogQuery::default()
        })
        .unwrap();
    assert_eq!(colored.items.len(), 1);
    assert_eq!(
        colored.items[0].styled.as_deref(),
        Some("\x1b[31mERROR\x1b[0m boom\x1b[0m")
    );
    assert_eq!(crate::strip_sgr("\x1b[1;31ma\x1b[0mb"), "ab");
}

#[test]
/// `head` returns the first lines of the range, and cannot go with `tail`.
fn head_returns_the_first_lines() {
    let (_directory, mut log) = store(LogOptions::default());
    log.ingest(b"one\ntwo\nthree\nfour\n", 1).unwrap();
    let page = log
        .query(&LogQuery {
            head: Some(2),
            ..LogQuery::default()
        })
        .unwrap();
    let texts: Vec<&str> = page.items.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(texts, ["one", "two"]);
    assert!(page.truncated);
    assert!(
        log.query(&LogQuery {
            head: Some(1),
            tail: Some(1),
            ..LogQuery::default()
        })
        .is_err()
    );
}
