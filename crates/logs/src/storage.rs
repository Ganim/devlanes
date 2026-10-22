use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use devlanes_proto::LogLine;
use serde::{Deserialize, Serialize};

use crate::{Error, Frame, Framer, Result, extract_level};

const MAX_DISK_LINE: u64 = (crate::MAX_LINE_BYTES * 4 + 256) as u64;
/// Held with `flock` by the one writer of a directory; readers never take it.
const LOCK_FILE: &str = ".writer.lock";

/// Bounds apply per run; the daemon applies aggregate policy across run directories.
#[derive(Debug, Clone)]
pub struct LogOptions {
    pub segment_bytes: u64,
    pub segment_age_ms: u64,
    pub retention_bytes: u64,
    pub retention_age_ms: u64,
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

/// The single writer of one unit directory, enforced by a lock file until `finish`.
pub struct UnitLog {
    pub(crate) unit: String,
    directory: PathBuf,
    options: LogOptions,
    /// Shared with reader snapshots; only the active (last) entry is ever mutated.
    segments: Vec<Arc<SegmentMetadata>>,
    writer: Option<BufWriter<File>>,
    framers: BTreeMap<String, Framer>,
    next_sequence: u64,
    failed: bool,
    lock: Option<File>,
}

/// A metadata snapshot of one unit directory. Queries through it never hold the
/// writer, and [`LogReader::open`] reads a directory without changing anything.
#[derive(Clone)]
pub struct LogReader {
    pub(crate) unit: String,
    pub(crate) segments: Vec<Arc<SegmentMetadata>>,
}

impl UnitLog {
    /// Takes the directory's writer lock, removes leftover temporaries, loads closed
    /// segments from their `.meta.json` and recovers only the final plain segment.
    /// Damaged segments are renamed to `*.corrupt` with a warning instead of failing.
    /// Returns [`Error::Locked`] while another writer owns the directory.
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
        let unit = validate_unit(unit.into())?;
        let directory = path.as_ref().to_owned();
        fs::create_dir_all(&directory)?;
        let lock = lock_directory(&directory)?;
        let mut paths = BTreeMap::<u64, PathBuf>::new();
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // Only a writer creates these, and holding the lock proves it is gone.
            if name.ends_with(".zst.tmp") || name.ends_with(".json.tmp") {
                remove_if_exists(&path)?;
                continue;
            }
            insert_segment(&mut paths, path);
        }
        let final_segment = paths.last_key_value().map(|(seq, _)| *seq);
        let mut segments: Vec<Arc<SegmentMetadata>> = Vec::new();
        let mut next_sequence = options.initial_sequence;
        for (first, path) in paths {
            let result = if segments.last().is_some_and(|p| first <= p.last_seq) {
                Err(Error::Corrupt(format!(
                    "{} overlaps the previous segment",
                    path.display()
                )))
            } else if Some(first) == final_segment && !is_compressed(&path) {
                recover_tail(&directory, &unit, options.compress, &path, first)
            } else {
                load_closed(&directory, &unit, &path, first).map(Some)
            };
            match result {
                Ok(Some(meta)) => {
                    next_sequence = next_sequence.max(
                        meta.last_seq
                            .checked_add(1)
                            .ok_or(Error::SequenceExhausted)?,
                    );
                    segments.push(Arc::new(meta));
                }
                Ok(None) => next_sequence = next_sequence.max(first),
                // Retention elsewhere removed it after the listing.
                Err(Error::Io(error)) if error.kind() == ErrorKind::NotFound => {
                    next_sequence = next_sequence.max(first);
                }
                Err(error) if is_damage(&error) => {
                    tracing::warn!(%error, path = %path.display(), "moving damaged log segment aside");
                    quarantine(&path)?;
                    quarantine(&directory.join(metadata_name(first)))?;
                    next_sequence = next_sequence.max(first.saturating_add(1));
                }
                Err(error) => return Err(error),
            }
        }
        let mut store = Self {
            unit,
            directory,
            options,
            segments,
            writer: None,
            framers: BTreeMap::new(),
            next_sequence,
            failed: false,
            lock: Some(lock),
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
        let mut framer = self.framers.remove(stream).unwrap_or_default();
        let result = framer.ingest(raw, timestamp_ms, |frame| self.append(frame));
        self.framers.insert(stream.to_owned(), framer);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Poll every 200 ms; `force` flushes all streams at child EOF before closing.
    /// Also runs `maintain` and flushes the file buffer so external readers stay current.
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
        self.maintain(now_ms)?;
        self.flush()
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
    /// The writer lock is released even when closing fails, so the next run can recover.
    pub fn finish(&mut self, now_ms: u64) -> Result<()> {
        let result = self
            .flush_partial(now_ms, true)
            .and_then(|()| self.rotate())
            .and_then(|()| self.retain(now_ms));
        self.writer = None;
        self.lock = None;
        result
    }

    /// Flushes if still writable and snapshots metadata. Query the returned reader
    /// after releasing any lock around this log so reads never block the writer.
    pub fn reader(&mut self) -> LogReader {
        // A failed flush marks the writer failed; the snapshot still serves what is on disk.
        if self.ensure_writable().is_ok() {
            let _ = self.flush();
        }
        LogReader {
            unit: self.unit.clone(),
            segments: self.segments.clone(),
        }
    }

    /// Removes the oldest closed segments until at most `max_bytes` remain on disk,
    /// closing the active segment as well when closed history alone is not enough.
    /// Returns the bytes left. The daemon uses this to apply its share of the total
    /// budget to a live unit, on the thread that owns the writer.
    pub fn trim_to(&mut self, max_bytes: u64) -> Result<u64> {
        self.ensure_writable()?;
        self.prune(max_bytes, |_| false)?;
        if self.disk_bytes() > max_bytes {
            self.rotate()?;
            self.prune(max_bytes, |_| false)?;
        }
        Ok(self.disk_bytes())
    }

    /// Bytes on disk across this directory's segments, compressed where closed.
    pub fn disk_bytes(&self) -> u64 {
        self.segments
            .iter()
            .fold(0u64, |sum, m| sum.saturating_add(m.bytes))
    }

    /// Copies of the current segment metadata, including the active segment;
    /// persist them after rotation.
    pub fn segments(&self) -> Vec<SegmentMetadata> {
        self.segments.iter().map(|m| (**m).clone()).collect()
    }

    /// Carry this value to the next run to preserve unit cursor identity.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
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
            styled: None,
        };
        // A colored line stores its styled form; readers strip it for `text`.
        let flags = u8::from(frame.partial)
            | (u8::from(frame.continued) << 1)
            | (u8::from(frame.styled.is_some()) << 2);
        let encoded = format!(
            "{}\t{}\t{}\t{}\t{}\n",
            line.seq,
            line.timestamp_ms,
            flags,
            line.level.as_deref().unwrap_or("-"),
            frame.styled.as_deref().unwrap_or(&line.text)
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
        // Copies the active entry once if a reader snapshot still shares it.
        let meta = Arc::make_mut(self.segments.last_mut().ok_or(Error::WriterFailed)?);
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
            .push(Arc::new(SegmentMetadata::empty(path, self.next_sequence)));
        Ok(())
    }

    /// Compression uses a temporary destination, retaining the source until rename.
    fn rotate(&mut self) -> Result<()> {
        if self.segments.last().is_none_or(|m| m.records == 0) {
            return Ok(());
        }
        self.flush()?;
        self.writer.take();
        let meta = Arc::make_mut(self.segments.last_mut().ok_or(Error::WriterFailed)?);
        close_segment(&self.directory, self.options.compress, meta)?;
        self.start_segment()
    }

    /// Active data is kept even when a single record exceeds the retention budget.
    pub fn retain(&mut self, now_ms: u64) -> Result<()> {
        let max_age = self.options.retention_age_ms;
        self.prune(self.options.retention_bytes, |oldest| {
            now_ms.saturating_sub(oldest.max_timestamp_ms) > max_age
        })
    }

    /// Removes closed segments, oldest first, while over `max_bytes` or `expired`.
    fn prune(&mut self, max_bytes: u64, expired: impl Fn(&SegmentMetadata) -> bool) -> Result<()> {
        let mut total = self.disk_bytes();
        while let Some(oldest) = self.segments.first() {
            if !oldest.closed || (total <= max_bytes && !expired(oldest)) {
                break;
            }
            remove_if_exists(&self.directory.join(metadata_name(oldest.first_seq)))?;
            remove_if_exists(&oldest.path)?;
            total = total.saturating_sub(oldest.bytes);
            self.segments.remove(0);
        }
        Ok(())
    }
}

impl LogReader {
    /// Reads a unit directory's history without recovering, locking, creating or
    /// removing anything. A missing directory is empty. A plain final segment without
    /// metadata (a live or crashed writer's tail) is scanned and its incomplete last
    /// record ignored; other unreadable segments are skipped with a warning.
    pub fn open(path: impl AsRef<Path>, unit: impl Into<String>) -> Result<Self> {
        let unit = validate_unit(unit.into())?;
        let entries = match fs::read_dir(path.as_ref()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(Self {
                    unit,
                    segments: Vec::new(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut paths = BTreeMap::new();
        for entry in entries {
            insert_segment(&mut paths, entry?.path());
        }
        let final_segment = paths.last_key_value().map(|(seq, _)| *seq);
        let mut segments: Vec<Arc<SegmentMetadata>> = Vec::new();
        for (first, path) in paths {
            let result = match load_metadata(&path, first) {
                Ok(meta) => Ok(meta),
                Err(_) if Some(first) == final_segment && !is_compressed(&path) => {
                    scan_segment(&path, &unit, first, true)
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(meta)
                    if meta.records > 0
                        && segments.last().is_none_or(|p| meta.first_seq > p.last_seq) =>
                {
                    segments.push(Arc::new(meta));
                }
                Ok(_) => {}
                // A live writer rotated or retention removed it after the listing.
                Err(Error::Io(error)) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "skipping unreadable log segment");
                }
            }
        }
        Ok(Self { unit, segments })
    }

    /// Segment metadata of this snapshot, oldest first.
    pub fn segments(&self) -> impl ExactSizeIterator<Item = &SegmentMetadata> {
        self.segments.iter().map(|m| &**m)
    }
}

/// Unit names become cursor prefixes, so they cannot contain the cursor separators.
fn validate_unit(unit: String) -> Result<String> {
    if unit.is_empty() || unit.contains('#') || unit.starts_with("before:") {
        return Err(Error::Options(
            "unit must be nonempty and cannot contain # or start with before:",
        ));
    }
    Ok(unit)
}

/// `flock` is released by the kernel when the file closes, including after a crash.
fn lock_directory(directory: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(LOCK_FILE))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(fs::TryLockError::WouldBlock) => Err(Error::Locked),
        Err(fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Records a `<seq>.log` or `<seq>.log.zst` path. A crash after compression's rename
/// can leave both; the plain source is authoritative until its removal commits.
fn insert_segment(paths: &mut BTreeMap<u64, PathBuf>, path: PathBuf) {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let stem = name
        .strip_suffix(".log.zst")
        .or_else(|| name.strip_suffix(".log"));
    if let Some(seq) = stem.and_then(|s| s.parse::<u64>().ok())
        && (!paths.contains_key(&seq) || !is_compressed(&path))
    {
        paths.insert(seq, path);
    }
}

fn is_compressed(path: &Path) -> bool {
    path.extension().is_some_and(|s| s == "zst")
}

fn metadata_name(first: u64) -> String {
    format!("{first:020}.meta.json")
}

/// Damage is quarantined; other errors (permissions, exhausted descriptors) propagate
/// so a transient failure never renames healthy history.
fn is_damage(error: &Error) -> bool {
    match error {
        Error::Corrupt(_) | Error::Metadata(_) => true,
        // zstd reports a corrupt frame as `Other`.
        Error::Io(error) => matches!(
            error.kind(),
            ErrorKind::InvalidData | ErrorKind::UnexpectedEof | ErrorKind::Other
        ),
        _ => false,
    }
}

/// Uses the stored index; only missing or stale metadata (a crash between compression
/// and writing it) costs a scan, after which the metadata is rewritten.
fn load_closed(directory: &Path, unit: &str, path: &Path, first: u64) -> Result<SegmentMetadata> {
    match load_metadata(path, first) {
        Ok(meta) => Ok(meta),
        Err(error) if is_damage(&error) || is_not_found(&error) => {
            let mut meta = scan_segment(path, unit, first, false)?;
            close_segment(directory, false, &mut meta)?;
            Ok(meta)
        }
        Err(error) => Err(error),
    }
}

/// Recovers the previous run's active segment: truncates a torn final record and
/// closes it. `None` means it was empty and was removed so its name can be reused.
fn recover_tail(
    directory: &Path,
    unit: &str,
    compress: bool,
    path: &Path,
    first: u64,
) -> Result<Option<SegmentMetadata>> {
    let duplicate = path.with_extension("log.zst");
    // A run with compression off leaves its closed tail plain, with metadata.
    if let Ok(meta) = load_metadata(path, first) {
        remove_if_exists(&duplicate)?;
        return Ok(Some(meta));
    }
    let mut meta = scan_segment(path, unit, first, true)?;
    OpenOptions::new()
        .write(true)
        .open(path)?
        .set_len(meta.uncompressed_bytes)?;
    remove_if_exists(&duplicate)?;
    if meta.records == 0 {
        fs::remove_file(path)?;
        return Ok(None);
    }
    close_segment(directory, compress, &mut meta)?;
    Ok(Some(meta))
}

/// Rebuilds metadata from records, requiring contiguous sequences from `first`.
fn scan_segment(
    path: &Path,
    unit: &str,
    first: u64,
    allow_partial_eof: bool,
) -> Result<SegmentMetadata> {
    let compressed = is_compressed(path);
    let mut meta = SegmentMetadata::empty(path.to_owned(), first);
    let mut reader = open_reader(path, compressed)?;
    while let Some((line, size)) = read_record(reader.as_mut(), unit, allow_partial_eof)? {
        let expected = if meta.records == 0 {
            first
        } else {
            meta.last_seq
                .checked_add(1)
                .ok_or(Error::SequenceExhausted)?
        };
        if line.seq != expected {
            return Err(Error::Corrupt(format!(
                "non-contiguous sequence in {}",
                path.display()
            )));
        }
        meta.record(&line, size);
    }
    meta.compressed = compressed;
    if compressed {
        meta.bytes = fs::metadata(path)?.len();
    }
    Ok(meta)
}

/// Closed indexes are trusted only after validating their ranges and actual path.
fn load_metadata(path: &Path, first: u64) -> Result<SegmentMetadata> {
    let directory = path.parent().ok_or(Error::Options("missing parent"))?;
    let file = File::open(directory.join(metadata_name(first)))?;
    let mut meta: SegmentMetadata = serde_json::from_reader(BufReader::new(file))?;
    let size = fs::metadata(path)?.len();
    let compressed = is_compressed(path);
    let valid = meta.closed
        && meta.compressed == compressed
        && meta.first_seq == first
        && meta.records > 0
        && meta
            .last_seq
            .checked_sub(first)
            .and_then(|n| n.checked_add(1))
            == Some(meta.records)
        && meta.bytes == size
        && (compressed || meta.uncompressed_bytes == size)
        && meta
            .index
            .first()
            .is_some_and(|entry| entry.seq == first && entry.offset == 0)
        && meta
            .index
            .windows(2)
            .all(|pair| pair[0].seq < pair[1].seq && pair[0].offset < pair[1].offset)
        && meta
            .index
            .iter()
            .all(|entry| entry.seq <= meta.last_seq && entry.offset < meta.uncompressed_bytes);
    if !valid {
        return Err(Error::Corrupt(format!(
            "invalid metadata for {}",
            path.display()
        )));
    }
    meta.path = path.to_owned();
    Ok(meta)
}

/// Keeps evidence for inspection; the `.corrupt` suffix hides it from every reader.
fn quarantine(path: &Path) -> Result<()> {
    let mut destination = path.as_os_str().to_owned();
    destination.push(".corrupt");
    match fs::rename(path, destination) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn is_not_found(error: &Error) -> bool {
    matches!(error, Error::Io(error) if error.kind() == ErrorKind::NotFound)
}

/// Retention and the daemon's maintenance pass may race on the same file.
fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
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
    let metadata_path = directory.join(metadata_name(meta.first_seq));
    let temporary = metadata_path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(meta)?)?;
    fs::rename(temporary, metadata_path)?;
    Ok(())
}

/// Decompression is streaming so a large segment never becomes a large allocation.
fn open_reader(path: &Path, compressed: bool) -> Result<Box<dyn BufRead>> {
    let file = File::open(path)?;
    if compressed {
        Ok(Box::new(BufReader::new(zstd::stream::read::Decoder::new(
            file,
        )?)))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

/// Discards the first `bytes` of a decompressed stream, failing if it is shorter.
pub(crate) fn skip(reader: &mut impl Read, bytes: u64) -> Result<()> {
    if std::io::copy(&mut reader.take(bytes), &mut std::io::sink())? != bytes {
        return Err(Error::Corrupt("index offset exceeds segment length".into()));
    }
    Ok(())
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
    let flags = parts
        .next()
        .and_then(|s| s.parse::<u8>().ok())
        .ok_or_else(|| Error::Corrupt("flags".into()))?;
    let level = parts.next().ok_or_else(|| Error::Corrupt("level".into()))?;
    let stored = parts.next().ok_or_else(|| Error::Corrupt("text".into()))?;
    let (text, styled) = if flags & 4 != 0 {
        (strip_sgr(stored), Some(stored.to_owned()))
    } else {
        (stored.to_owned(), None)
    };
    Ok(Some((
        LogLine {
            unit: unit.to_owned(),
            seq,
            timestamp_ms,
            text,
            level: (level != "-").then(|| level.to_owned()),
            styled,
        },
        size as u64,
    )))
}

/// `text` without its `ESC [ … m` sequences: the only escapes stored lines keep.
pub fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("\x1b[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('m') {
            Some(end) => rest = &after[end + 1..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}
