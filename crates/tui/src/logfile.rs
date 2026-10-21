//! Follows the daemon's own log file. Blocking; the runtime calls it from
//! `spawn_blocking`.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

/// Bytes read when starting or after the file was rotated or truncated.
const FIRST_READ: u64 = 256 * 1024;
/// Upper bound per read, so a burst of logging cannot stall the UI.
const MAX_READ: u64 = 4 * 1024 * 1024;

/// New complete lines since the last read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogChunk {
    pub lines: Vec<String>,
    /// Where the next read starts; just past the last complete line.
    pub offset: u64,
    /// True when `lines` replace everything shown (first read, rotation).
    pub reset: bool,
}

/// Reads complete lines from `offset`; `0` reads the tail of the file. A file
/// shorter than `offset` was rotated or truncated, so its tail is read again.
pub fn read(path: &Path, offset: u64) -> io::Result<LogChunk> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let (mut start, mut reset) = if offset == 0 || len < offset {
        (len.saturating_sub(FIRST_READ), true)
    } else {
        (offset, false)
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
        offset: start + u64::try_from(complete).unwrap_or(0),
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
        let first = read(&path, 0)?;
        assert_eq!(first.lines, ["one", "two"]);
        assert!(first.reset);
        write!(file, "ial\nthree\n")?;
        let next = read(&path, first.offset)?;
        assert_eq!(next.lines, ["partial", "three"]);
        assert!(!next.reset);
        // Rotation leaves a shorter file behind.
        let mut file = File::create(&path)?;
        writeln!(file, "fresh")?;
        let rotated = read(&path, next.offset)?;
        assert_eq!(rotated.lines, ["fresh"]);
        assert!(rotated.reset);
        Ok(())
    }
}
