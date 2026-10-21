//! Follows the daemon's own log file. Blocking; the runtime calls it from
//! `spawn_blocking`.

use devlanes_sys::FileIdentity;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

/// Bytes read when starting or after the file was rotated or truncated.
const FIRST_READ: u64 = 256 * 1024;
/// Upper bound per read, so a burst of logging cannot stall the UI.
const MAX_READ: u64 = 4 * 1024 * 1024;

/// Where the last read stopped, and in which file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogPosition {
    pub offset: u64,
    /// File identity (device and inode on Unix); a rotated log is a different
    /// file at the same path.
    pub file: FileIdentity,
}

/// New complete lines since the last read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogChunk {
    pub lines: Vec<String>,
    /// Where the next read starts; just past the last complete line.
    pub position: LogPosition,
    /// True when `lines` replace everything shown (first read, rotation).
    pub reset: bool,
}

/// Reads complete lines after `from`; the default position reads the tail of
/// the file. A different file at the path, or one shorter than the offset,
/// was rotated or truncated, so its tail is read again.
pub fn read(path: &Path, from: LogPosition) -> io::Result<LogChunk> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();
    let identity = devlanes_sys::file_identity(&file)?;
    let same_file = identity == from.file && from.offset > 0 && len >= from.offset;
    let (mut start, mut reset) = if same_file {
        (from.offset, false)
    } else {
        (len.saturating_sub(FIRST_READ), true)
    };
    if len - start > MAX_READ {
        start = len - MAX_READ;
        reset = true;
    }
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::new();
    file.take(len - start).read_to_end(&mut buffer)?;
    // An unfinished last line is read again next time, once it is complete.
    let complete = buffer
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let text = String::from_utf8_lossy(&buffer[..complete]);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    // A read that starts mid-file starts mid-line.
    if reset && start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(LogChunk {
        lines,
        position: LogPosition {
            offset: start + u64::try_from(complete).unwrap_or(0),
            file: identity,
        },
        reset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn follows_appends_and_restarts_after_truncation() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("daemon.log");
        let mut file = File::create(&path)?;
        write!(file, "one\ntwo\npart")?;
        let first = read(&path, LogPosition::default())?;
        assert_eq!(first.lines, ["one", "two"]);
        assert!(first.reset);
        write!(file, "ial\nthree\n")?;
        let next = read(&path, first.position)?;
        assert_eq!(next.lines, ["partial", "three"]);
        assert!(!next.reset);
        // Truncation in place leaves a shorter file behind.
        let mut file = File::create(&path)?;
        writeln!(file, "fresh")?;
        let truncated = read(&path, next.position)?;
        assert_eq!(truncated.lines, ["fresh"]);
        assert!(truncated.reset);
        Ok(())
    }

    #[test]
    fn a_rotated_file_that_grew_past_the_offset_is_read_from_its_tail() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "old 1\nold 2\n")?;
        let first = read(&path, LogPosition::default())?;
        // Rotation: the old file moves away and a new, longer one takes the path.
        std::fs::rename(&path, dir.path().join("daemon.log.1"))?;
        std::fs::write(&path, "new 1\nnew 2\nnew 3\nnew 4\n")?;
        let rotated = read(&path, first.position)?;
        assert!(rotated.reset);
        assert_eq!(rotated.lines, ["new 1", "new 2", "new 3", "new 4"]);
        Ok(())
    }
}
