use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use devlanes_proto::LogLine;
use serde::{Deserialize, Serialize};

use crate::{Error, Frame, Framer, RawRing, Result, extract_level};

const MAX_DISK_LINE: u64 = (crate::MAX_LINE_BYTES * 4 + 256) as u64;

/// Bounds apply per run; the daemon applies aggregate policy across run directories.
#[derive(Debug, Clone)]
pub struct LogOptions {
    pub segment_bytes: u64,
    pub segment_age_ms: u64,
    pub retention_bytes: u64,
    pub retention_age_ms: u64,
    pub raw_ring_bytes: usize,
    /// Supply the previous run's next sequence to keep unit cursors stable.
    pub initial_sequence: u64,
    pub compress: bool,
}

impl Default for LogOptions {
    /// Defaults keep a development run bounded without requiring configuration.
    fn default() -> Self {
        Self {
            segment_bytes: 16 * 1024 * 1024,
            segment_age_ms: 60 * 60 * 1000,
            retention_bytes: 512 * 1024 * 1024,
            retention_age_ms: 7 * 24 * 60 * 60 * 1000,
            raw_ring_bytes: 16 * 1024 * 1024,
            initial_sequence: 1,
            compress: true,
        }
    }
}

/// Sparse offsets address uncompressed bytes, including in a zstd stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub seq: u64,
    pub timestamp_ms: u64,
    pub offset: u64,
}

/// Snapshots can be persisted in `log_segments` without opening SQLite here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMetadata {
    pub path: PathBuf,
    pub first_seq: u64,
    pub last_seq: u64,
    pub min_timestamp_ms: u64,
    pub max_timestamp_ms: u64,
    pub bytes: u64,
    pub uncompressed_bytes: u64,
    pub records: u64,
    pub compressed: bool,
    pub closed: bool,
    pub index: Vec<IndexEntry>,
}

impl SegmentMetadata {
    /// Empty segment names reserve the next sequence across retention and restarts.
    fn empty(path: PathBuf, seq: u64) -> Self {
        Self {
            path,
            first_seq: seq,
            last_seq: seq.saturating_sub(1),
            min_timestamp_ms: u64::MAX,
            max_timestamp_ms: 0,
            bytes: 0,
            uncompressed_bytes: 0,
            records: 0,
            compressed: false,
            closed: false,
            index: Vec::new(),
        }
    }

    /// Receive timestamps may move backwards after a wall-clock adjustment.
    fn record(&mut self, line: &LogLine, bytes: u64) {
        if self.records.is_multiple_of(256) {
            self.index.push(IndexEntry {
                seq: line.seq,
                timestamp_ms: line.timestamp_ms,
                offset: self.uncompressed_bytes,
            });
        }
        self.last_seq = line.seq;
        self.min_timestamp_ms = self.min_timestamp_ms.min(line.timestamp_ms);
        self.max_timestamp_ms = self.max_timestamp_ms.max(line.timestamp_ms);
        self.records += 1;
        self.uncompressed_bytes += bytes;
        self.bytes = self.uncompressed_bytes;
    }
}

/// One owner per run directory; concurrent writers to the same directory are invalid.
pub struct UnitLog {
    pub(crate) unit: String,
    directory: PathBuf,
    options: LogOptions,
    pub(crate) segments: Vec<SegmentMetadata>,
    writer: Option<BufWriter<File>>,
    framers: BTreeMap<String, Framer>,
    raw: RawRing,
    next_sequence: u64,
    failed: bool,
}

impl UnitLog {
    /// Recovers indexes from records, accepting only an incomplete final plain record.
    /// The caller must serialize access and provide an exclusive per-run directory.
    pub fn open(
        path: impl AsRef<Path>,
        unit: impl Into<String>,
        options: LogOptions,
    ) -> Result<Self> {
        if options.segment_bytes == 0
            || options.segment_age_ms == 0
            || options.initial_sequence == 0
        {
            return Err(Error::Options(
                "segment bounds and initial sequence must be positive",
            ));
        }
        let unit = unit.into();
        if unit.is_empty() || unit.contains('#') || unit.starts_with("before:") {
            return Err(Error::Options(
                "unit must be nonempty and cannot contain # or start with before:",
            ));
        }
        fs::create_dir_all(path.as_ref())?;
        let mut paths = BTreeMap::<u64, PathBuf>::new();
        for entry in fs::read_dir(path.as_ref())? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let stem = name
                .strip_suffix(".log.zst")
                .or_else(|| name.strip_suffix(".log"));
            if let Some(seq) = stem.and_then(|s| s.parse::<u64>().ok()) {
                // A crash after compression rename can leave both files; the plain
                // source is authoritative until removal commits compression.
                if !paths.contains_key(&seq) || path.extension().is_some_and(|s| s == "log") {
                    paths.insert(seq, path);
                }
            }
        }
        let mut segments = Vec::new();
        let mut next_sequence = options.initial_sequence;
        let final_segment = paths.last_key_value().map(|(seq, _)| *seq);
        for (first, path) in paths {
            let compressed = path.extension().is_some_and(|s| s == "zst");
            let mut meta = SegmentMetadata::empty(path.clone(), first);
            let mut reader = open_reader(&path, compressed)?;
            let mut offset = 0;
            while let Some((line, size)) = read_record(
                &mut reader,
                &unit,
                !compressed && Some(first) == final_segment,
            )? {
                if (meta.records == 0 && line.seq != first)
                    || (meta.records > 0 && Some(line.seq) != meta.last_seq.checked_add(1))
                    || segments
                        .last()
                        .is_some_and(|previous: &SegmentMetadata| line.seq <= previous.last_seq)
                {
                    return Err(Error::Corrupt(format!(
                        "non-contiguous sequence in {}",
                        path.display()
                    )));
                }
                meta.record(&line, size);
                offset += size;
            }
            if !compressed {
                OpenOptions::new()
                    .write(true)
                    .open(&path)?
                    .set_len(offset)?;
                let duplicate = path.with_extension("log.zst");
                if duplicate.exists() {
                    fs::remove_file(duplicate)?;
                }
            }
            meta.bytes = fs::metadata(&path)?.len();
            meta.compressed = compressed;
            meta.closed = true;
            next_sequence = next_sequence.max(if meta.records == 0 {
                first
            } else {
                meta.last_seq
                    .checked_add(1)
                    .ok_or(Error::SequenceExhausted)?
            });
            // Reuse an empty tail rather than attempting create_new on its name.
            if meta.records == 0 {
                fs::remove_file(&path)?;
            } else {
                close_segment(
                    path.parent()
                        .ok_or(Error::Options("run directory has no parent"))?,
                    options.compress,
                    &mut meta,
                )?;
                segments.push(meta);
            }
        }
        let mut store = Self {
            unit,
            directory: path.as_ref().to_owned(),
            raw: RawRing::new(options.raw_ring_bytes),
            options,
            segments,
            writer: None,
            framers: BTreeMap::new(),
            next_sequence,
            failed: false,
        };
        store.start_segment()?;
        Ok(store)
    }

    /// Ingests the default merged stream; pipe callers should use `ingest_stream`.
    /// On write failure reopen the store before retrying; consumed input is not replayed.
    pub fn ingest(&mut self, raw: &[u8], timestamp_ms: u64) -> Result<()> {
        self.ingest_stream("merged", raw, timestamp_ms)
    }

    /// Separate framers prevent stdout prompts from being joined to stderr lines.
    pub fn ingest_stream(&mut self, stream: &str, raw: &[u8], timestamp_ms: u64) -> Result<()> {
        self.ensure_writable()?;
        if self.framers.len() >= 16 && !self.framers.contains_key(stream) {
            return Err(Error::Options(
                "a unit may have at most 16 independent log streams",
            ));
        }
        self.raw.push(raw);
        let mut framer = self.framers.remove(stream).unwrap_or_default();
        let result = framer.ingest(raw, timestamp_ms, |frame| self.append(frame));
        self.framers.insert(stream.to_owned(), framer);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Poll every 200 ms; `force` flushes all streams at child EOF before closing.
    pub fn flush_partial(&mut self, now_ms: u64, force: bool) -> Result<()> {
        self.ensure_writable()?;
        let streams: Vec<_> = self.framers.keys().cloned().collect();
        for stream in streams {
            if let Some(frame) = self
                .framers
                .get_mut(&stream)
                .and_then(|f| f.flush_partial(now_ms, force))
                && let Err(error) = self.append(frame)
            {
                self.failed = true;
                return Err(error);
            }
        }
        self.maintain(now_ms)
    }

    /// Rotation by age also runs during idle periods when the daemon polls its timer.
    pub fn maintain(&mut self, now_ms: u64) -> Result<()> {
        self.ensure_writable()?;
        if self.segments.last().is_some_and(|m| {
            m.records > 0
                && now_ms.saturating_sub(m.min_timestamp_ms) >= self.options.segment_age_ms
        }) {
            self.rotate()?;
        }
        self.retain(now_ms)
    }

    /// Flushes buffered records so independent readers can inspect current disk logs.
    pub fn flush(&mut self) -> Result<()> {
        self.ensure_writable()?;
        if let Some(writer) = self.writer.as_mut()
            && let Err(error) = writer.flush()
        {
            self.failed = true;
            return Err(error.into());
        }
        Ok(())
    }

    /// Closes and optionally compresses the active segment after forced partial flush.
    /// An empty tail file reserves the next sequence even if retention removes history.
    pub fn finish(&mut self, now_ms: u64) -> Result<()> {
        self.flush_partial(now_ms, true)?;
        self.rotate()?;
        self.retain(now_ms)
    }

    /// Current snapshots include the active segment; persist them after rotation.
    pub fn segments(&self) -> &[SegmentMetadata] {
        &self.segments
    }

    /// Carry this value to the next run to preserve unit cursor identity.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Terminal replay includes ANSI bytes and bytes not yet framed as a line.
    pub fn raw_snapshot(&self) -> Vec<u8> {
        self.raw.snapshot()
    }

    /// Stable paths let clients run external grep without owning storage policy.
    pub fn path(&self) -> &Path {
        &self.directory
    }

    /// A failed append may have partially written, so continuing could duplicate seqs.
    fn ensure_writable(&self) -> Result<()> {
        if self.failed || self.writer.is_none() {
            Err(Error::WriterFailed)
        } else {
            Ok(())
        }
    }

    /// The text prefix remains grep-able; flags preserve partial/continued boundaries.
    fn append(&mut self, frame: Frame) -> Result<()> {
        let next = self
            .next_sequence
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        let line = LogLine {
            unit: self.unit.clone(),
            seq: self.next_sequence,
            timestamp_ms: frame.timestamp_ms,
            level: extract_level(&frame.text),
            text: frame.text,
        };
        let flags = u8::from(frame.partial) | (u8::from(frame.continued) << 1);
        let encoded = format!(
            "{}\t{}\t{}\t{}\t{}\n",
            line.seq,
            line.timestamp_ms,
            flags,
            line.level.as_deref().unwrap_or("-"),
            line.text
        );
        if self.segments.last().is_some_and(|m| {
            m.records > 0
                && (m.uncompressed_bytes.saturating_add(encoded.len() as u64)
                    > self.options.segment_bytes
                    || line.timestamp_ms.saturating_sub(m.min_timestamp_ms)
                        >= self.options.segment_age_ms)
        }) {
            self.rotate()?;
        }
        let writer = self.writer.as_mut().ok_or(Error::WriterFailed)?;
        writer.write_all(encoded.as_bytes())?;
        let meta = self.segments.last_mut().ok_or(Error::WriterFailed)?;
        meta.record(&line, encoded.len() as u64);
        self.next_sequence = next;
        self.retain(line.timestamp_ms)
    }

    /// Segment creation uses exclusive create to catch accidental competing owners.
    fn start_segment(&mut self) -> Result<()> {
        let path = self
            .directory
            .join(format!("{:020}.log", self.next_sequence));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        self.writer = Some(BufWriter::new(file));
        self.segments
            .push(SegmentMetadata::empty(path, self.next_sequence));
        Ok(())
    }

    /// Compression uses a temporary destination, retaining the source until rename.
    fn rotate(&mut self) -> Result<()> {
        if self.segments.last().is_none_or(|m| m.records == 0) {
            return Ok(());
        }
        self.flush()?;
        self.writer.take();
        let meta = self.segments.last_mut().ok_or(Error::WriterFailed)?;
        close_segment(&self.directory, self.options.compress, meta)?;
        self.start_segment()
    }

    /// Active data is kept even when a single record exceeds the retention budget.
    pub fn retain(&mut self, now_ms: u64) -> Result<()> {
        let mut total = self
            .segments
            .iter()
            .fold(0u64, |sum, m| sum.saturating_add(m.bytes));
        while let Some(oldest) = self.segments.first() {
            if !oldest.closed
                || (total <= self.options.retention_bytes
                    && now_ms.saturating_sub(oldest.max_timestamp_ms)
                        <= self.options.retention_age_ms)
            {
                break;
            }
            let metadata_path = self
                .directory
                .join(format!("{:020}.meta.json", oldest.first_seq));
            match fs::remove_file(metadata_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            fs::remove_file(&oldest.path)?;
            total = total.saturating_sub(oldest.bytes);
            self.segments.remove(0);
        }
        Ok(())
    }
}

/// Recovery uses the same atomic compression sequence as normal rotation.
fn close_segment(directory: &Path, compress: bool, meta: &mut SegmentMetadata) -> Result<()> {
    meta.closed = true;
    if compress && !meta.compressed {
        let destination = meta.path.with_extension("log.zst");
        let temporary = destination.with_extension("zst.tmp");
        let mut source = File::open(&meta.path)?;
        let mut target = File::create(&temporary)?;
        zstd::stream::copy_encode(&mut source, &mut target, 3)?;
        target.flush()?;
        fs::rename(&temporary, &destination)?;
        fs::remove_file(&meta.path)?;
        meta.path = destination;
        meta.compressed = true;
        meta.bytes = fs::metadata(&meta.path)?.len();
    }
    let metadata_path = directory.join(format!("{:020}.meta.json", meta.first_seq));
    let temporary = metadata_path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(meta)?)?;
    fs::rename(temporary, metadata_path)?;
    Ok(())
}

/// Decompression is streaming so a large segment never becomes a large allocation.
pub(crate) fn open_reader(path: &Path, compressed: bool) -> Result<Box<dyn BufRead>> {
    open_reader_at(path, compressed, 0)
}

/// Plain files seek directly; zstd streams discard preceding decompressed bytes.
pub(crate) fn open_reader_at(
    path: &Path,
    compressed: bool,
    offset: u64,
) -> Result<Box<dyn BufRead>> {
    let mut file = File::open(path)?;
    if compressed {
        let mut reader = BufReader::new(zstd::stream::read::Decoder::new(file)?);
        if std::io::copy(&mut reader.by_ref().take(offset), &mut std::io::sink())? != offset {
            return Err(Error::Corrupt("index offset exceeds segment length".into()));
        }
        Ok(Box::new(reader))
    } else {
        file.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(BufReader::new(file)))
    }
}

/// Record reads are bounded even if the file was corrupted or edited externally.
pub(crate) fn read_record(
    reader: &mut dyn BufRead,
    unit: &str,
    allow_partial_eof: bool,
) -> Result<Option<(LogLine, u64)>> {
    let mut bytes = Vec::new();
    let size = reader.take(MAX_DISK_LINE).read_until(b'\n', &mut bytes)?;
    if size == 0 {
        return Ok(None);
    }
    if bytes.last() != Some(&b'\n') {
        if size as u64 == MAX_DISK_LINE || !allow_partial_eof {
            return Err(Error::Corrupt("unterminated or oversized record".into()));
        }
        return Ok(None);
    }
    bytes.pop();
    let text = std::str::from_utf8(&bytes).map_err(|e| Error::Corrupt(e.to_string()))?;
    let mut parts = text.splitn(5, '\t');
    let seq = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Corrupt("sequence".into()))?;
    let timestamp_ms = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Corrupt("timestamp".into()))?;
    let _flags = parts
        .next()
        .and_then(|s| s.parse::<u8>().ok())
        .ok_or_else(|| Error::Corrupt("flags".into()))?;
    let level = parts.next().ok_or_else(|| Error::Corrupt("level".into()))?;
    let text = parts
        .next()
        .ok_or_else(|| Error::Corrupt("text".into()))?
        .to_owned();
    Ok(Some((
        LogLine {
            unit: unit.to_owned(),
            seq,
            timestamp_ms,
            text,
            level: (level != "-").then(|| level.to_owned()),
        },
        size as u64,
    )))
}
