//! `devlanes daemon logs`: prints the end of the daemon's log file and, when
//! following, what is appended to it, across rotations. Reads the file directly
//! so it works on every OS and while the daemon is wedged. Blocking.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use devlanes_sys::FileIdentity;

/// How often a follow checks for appended data and rotation.
pub const POLL: Duration = Duration::from_millis(250);

/// Bytes read per step while looking backwards for the last lines.
const BACKWARD_CHUNK: u64 = 64 * 1024;

/// Size of the forward read buffer, allocated once per follower.
const FORWARD_CHUNK: usize = 64 * 1024;

/// The log file being printed: which file and how far. Output is printed as it
/// arrives, an unfinished last line included, like `tail -f`.
pub struct Follower {
    path: PathBuf,
    /// The open file; `None` until the path exists.
    file: Option<File>,
    identity: FileIdentity,
    offset: u64,
    /// Reused by every forward read; empty until the first one.
    buffer: Vec<u8>,
    /// Whether the last byte printed was not a newline. The line is ended
    /// before output from another file (rotation, truncation) and at the end.
    owed: bool,
    text: os::Text,
}

impl Follower {
    /// Opens `path` and prints its last `lines` lines to `out`, an unfinished
    /// last line included. A missing file prints nothing and is picked up once
    /// it appears.
    pub fn start(path: &Path, lines: usize, out: &mut impl Write) -> io::Result<Self> {
        let mut follower = Self {
            path: path.to_owned(),
            file: None,
            identity: FileIdentity::default(),
            offset: 0,
            buffer: Vec::new(),
            owed: false,
            text: os::Text::new(),
        };
        let Some(mut file) = open(path)? else {
            return Ok(follower);
        };
        let len = file.metadata()?.len();
        follower.offset = tail_start(&mut file, len, lines)?;
        follower.identity = devlanes_sys::file_identity(&file)?;
        follower.file = Some(file);
        follower.read_to_end(out)?;
        Ok(follower)
    }

    /// Whether the log file existed when last looked at.
    pub fn found(&self) -> bool {
        self.file.is_some()
    }

    /// Prints what was appended since the last call. A file truncated in place
    /// is read again from its start. When another file now sits at the path
    /// (rotation, or a log that only now appeared), the old one is read to its
    /// end once more and the new one printed from its start, on a new line.
    pub fn poll(&mut self, out: &mut impl Write) -> io::Result<()> {
        let replaced = match devlanes_sys::path_identity(&self.path) {
            Ok(identity) => self.file.is_none() || identity != self.identity,
            // Between a rotation's rename and the new file: keep the old one.
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        let next = if replaced { open(&self.path)? } else { None };
        // The daemon may have written to the old file after the last poll and
        // before it was replaced.
        self.read_to_end(out)?;
        let Some(file) = next else {
            return Ok(());
        };
        self.end_line(out)?;
        self.identity = devlanes_sys::file_identity(&file)?;
        self.file = Some(file);
        self.offset = 0;
        self.read_to_end(out)
    }

    /// Ends an unfinished last line with a newline and flushes.
    pub fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.end_line(out)?;
        out.flush()
    }

    /// Prints what is held back for an incomplete character, then the newline
    /// the last printed line still needs, if any.
    fn end_line(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.text.end(out)?;
        if self.owed {
            out.write_all(b"\n")?;
            self.owed = false;
        }
        Ok(())
    }

    /// Prints everything from the offset to the current end of the file.
    fn read_to_end(&mut self, out: &mut impl Write) -> io::Result<()> {
        let Some(file) = self.file.as_ref() else {
            return Ok(());
        };
        let len = file.metadata()?.len();
        if len < self.offset {
            // Truncated in place: start over, on a line of its own.
            self.offset = 0;
            self.end_line(out)?;
        }
        if len == self.offset {
            return Ok(());
        }
        if self.buffer.is_empty() {
            self.buffer = vec![0; FORWARD_CHUNK];
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.seek(SeekFrom::Start(self.offset))?;
        loop {
            let read = file.read(&mut self.buffer)?;
            let Some(chunk) = self.buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
                break;
            };
            self.offset += u64::try_from(read).unwrap_or(u64::MAX);
            self.owed = chunk.last() != Some(&b'\n');
            self.text.write(out, chunk)?;
        }
        out.flush()
    }
}

/// Prints the last `lines` lines of `path`; with `follow`, keeps printing
/// what is appended (checking every `poll`) for as long as `keep_going` says.
/// Without `follow` a missing file is an error.
pub fn print(
    path: &Path,
    lines: usize,
    follow: bool,
    poll: Duration,
    out: &mut impl Write,
    mut keep_going: impl FnMut() -> bool,
) -> io::Result<()> {
    let mut follower = Follower::start(path, lines, out)?;
    if !follow {
        if !follower.found() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: no such file", path.display()),
            ));
        }
        return follower.finish(out);
    }
    while keep_going() {
        std::thread::sleep(poll);
        follower.poll(out)?;
    }
    follower.finish(out)
}

/// `path` opened for reading, or `None` when it does not exist. std opens with
/// delete sharing on Windows, so the daemon can still rotate the file.
fn open(path: &Path) -> io::Result<Option<File>> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Where the last `lines` lines of the first `len` bytes of `file` start. An
/// unfinished last line counts as a line, as with `tail -n`.
fn tail_start(file: &mut File, len: u64, lines: usize) -> io::Result<u64> {
    if lines == 0 {
        return Ok(len);
    }
    let mut newlines = 0;
    let mut end = len;
    let mut buffer = Vec::new();
    while end > 0 {
        let start = end.saturating_sub(BACKWARD_CHUNK);
        buffer.resize(usize::try_from(end - start).unwrap_or(0), 0);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer)?;
        for (index, byte) in buffer.iter().enumerate().rev() {
            let position = start + u64::try_from(index).unwrap_or(0);
            // The file's final newline ends the last line instead of starting one.
            if *byte == b'\n' && position + 1 < len {
                newlines += 1;
                if newlines == lines {
                    return Ok(position + 1);
                }
            }
        }
        end = start;
    }
    Ok(0)
}

/// Unix terminals take any bytes, so the log is printed exactly as written.
#[cfg(unix)]
mod os {
    use std::io::{self, Write};

    /// Passes bytes through.
    pub struct Text;

    impl Text {
        /// A pass-through writer.
        pub fn new() -> Self {
            Self
        }

        /// Writes `bytes` unchanged.
        pub fn write(&mut self, out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
            out.write_all(bytes)
        }

        /// Nothing is ever held back.
        pub fn end(&mut self, _out: &mut impl Write) -> io::Result<()> {
            Ok(())
        }
    }
}

/// Windows consoles reject writes that are not valid UTF-8, so invalid bytes
/// are replaced. A character split across two reads is held back until its
/// last byte arrives instead of becoming two replacement characters.
#[cfg(windows)]
mod os {
    use std::io::{self, Write};

    /// Converts to UTF-8, holding an incomplete trailing character.
    pub struct Text {
        held: Vec<u8>,
    }

    impl Text {
        /// A converter holding nothing yet.
        pub fn new() -> Self {
            Self { held: Vec::new() }
        }

        /// Writes `bytes` (after what was held) as UTF-8, replacing invalid
        /// sequences and keeping an incomplete last character for later.
        pub fn write(&mut self, out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
            self.held.extend_from_slice(bytes);
            let complete = self.held.len() - incomplete_tail(&self.held);
            out.write_all(String::from_utf8_lossy(&self.held[..complete]).as_bytes())?;
            self.held.drain(..complete);
            Ok(())
        }

        /// Writes what is held (an incomplete character) replaced, since no
        /// more bytes will complete it.
        pub fn end(&mut self, out: &mut impl Write) -> io::Result<()> {
            if !self.held.is_empty() {
                out.write_all(String::from_utf8_lossy(&self.held).as_bytes())?;
                self.held.clear();
            }
            Ok(())
        }
    }

    /// How many bytes at the end of `bytes` start a UTF-8 character that
    /// needs more bytes than are there; 0 when the end is complete or invalid.
    fn incomplete_tail(bytes: &[u8]) -> usize {
        for back in 1..=bytes.len().min(3) {
            let byte = bytes[bytes.len() - back];
            // A continuation byte: the character starts further back.
            if byte & 0b1100_0000 == 0b1000_0000 {
                continue;
            }
            let needed = match byte {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => 1,
            };
            return if needed > back { back } else { 0 };
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `follower.poll` prints, as raw bytes.
    fn polled_bytes(follower: &mut Follower) -> Vec<u8> {
        let mut out = Vec::new();
        follower.poll(&mut out).expect("poll");
        out
    }

    /// What `follower.poll` prints.
    fn polled(follower: &mut Follower) -> String {
        String::from_utf8(polled_bytes(follower)).expect("utf-8")
    }

    /// Appends `bytes` to `path`.
    fn append(path: &Path, bytes: impl AsRef<[u8]>) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .expect("open");
        file.write_all(bytes.as_ref()).expect("append");
    }

    #[test]
    /// The last N lines, counting an unfinished last line, like `tail -n N`.
    fn prints_the_last_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "one\ntwo\nthree\n").expect("write");
        let printed = |lines: usize| {
            let mut out = Vec::new();
            print(&path, lines, false, POLL, &mut out, || false).expect("print");
            String::from_utf8(out).expect("utf-8")
        };
        assert_eq!(printed(2), "two\nthree\n");
        assert_eq!(printed(3), "one\ntwo\nthree\n");
        assert_eq!(printed(10), "one\ntwo\nthree\n");
        assert_eq!(printed(0), "");
        std::fs::write(&path, "one\ntwo\npart").expect("write");
        assert_eq!(printed(1), "part\n");
        assert_eq!(printed(2), "two\npart\n");
        std::fs::write(&path, "").expect("write");
        assert_eq!(printed(5), "");
    }

    #[test]
    /// Lines spread over several backward reads are still found.
    fn finds_lines_across_chunks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        let long = "x".repeat(usize::try_from(BACKWARD_CHUNK).expect("fits") + 10);
        std::fs::write(&path, format!("first\n{long}\nlast\n")).expect("write");
        let mut out = Vec::new();
        print(&path, 2, false, POLL, &mut out, || false).expect("print");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            format!("{long}\nlast\n")
        );
    }

    #[test]
    /// Without follow a missing log is an error.
    fn missing_file_without_follow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut out = Vec::new();
        let error = print(
            &dir.path().join("daemon.log"),
            10,
            false,
            POLL,
            &mut out,
            || false,
        )
        .expect_err("missing");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    /// Appended output prints at once, an unfinished line included; a file
    /// truncated in place starts over on a line of its own.
    fn follows_appends_and_truncation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "old\n").expect("write");
        let mut out = Vec::new();
        let mut follower = Follower::start(&path, 10, &mut out).expect("start");
        assert_eq!(String::from_utf8(out).expect("utf-8"), "old\n");
        assert_eq!(polled(&mut follower), "");
        append(&path, "new\npar");
        assert_eq!(polled(&mut follower), "new\npar");
        append(&path, "tial\n");
        assert_eq!(polled(&mut follower), "tial\n");
        std::fs::write(&path, "fresh\n").expect("truncate");
        assert_eq!(polled(&mut follower), "fresh\n");
        append(&path, "cut");
        assert_eq!(polled(&mut follower), "cut");
        std::fs::write(&path, "x\n").expect("truncate");
        assert_eq!(polled(&mut follower), "\nx\n");
    }

    #[test]
    /// A rotation prints the rest of the old file, including what was written
    /// to it after the last poll, then the new one from its start on a new
    /// line, even while the follower holds the old file open.
    fn follows_rotation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        let rotated = dir.path().join("daemon.log.1");
        std::fs::write(&path, "a\n").expect("write");
        let mut out = Vec::new();
        let mut follower = Follower::start(&path, 10, &mut out).expect("start");
        append(&path, "b\nunfinished");
        // The daemon renames the log while this reader has it open, as it does
        // at startup, so the rename must work on Windows too.
        devlanes_sys::rename_retrying(&path, &rotated).expect("rotate");
        // Between the rename and the new file the old one is still read.
        assert_eq!(polled(&mut follower), "b\nunfinished");
        append(&rotated, " line\nlast");
        std::fs::write(&path, "c\n").expect("new log");
        assert_eq!(polled(&mut follower), " line\nlast\nc\n");
        append(&path, "d\n");
        assert_eq!(polled(&mut follower), "d\n");
    }

    #[test]
    /// A follow that starts before the daemon wrote its log picks it up.
    fn follows_a_log_that_appears_later() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        let mut out = Vec::new();
        let mut follower = Follower::start(&path, 10, &mut out).expect("start");
        assert!(!follower.found());
        assert_eq!(polled(&mut follower), "");
        std::fs::write(&path, "hello\n").expect("write");
        assert_eq!(polled(&mut follower), "hello\n");
        assert!(follower.found());
    }

    #[test]
    /// Polls that find nothing new read nothing, and the read buffer is
    /// allocated once and then reused.
    fn reads_reuse_one_buffer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "").expect("write");
        let mut out = Vec::new();
        let mut follower = Follower::start(&path, 10, &mut out).expect("start");
        assert_eq!(polled(&mut follower), "");
        assert_eq!(follower.buffer.capacity(), 0);
        append(&path, "one\n");
        assert_eq!(polled(&mut follower), "one\n");
        let buffer = follower.buffer.as_ptr();
        append(&path, "two\n");
        assert_eq!(polled(&mut follower), "two\n");
        assert_eq!(follower.buffer.as_ptr(), buffer);
        assert_eq!(follower.buffer.len(), FORWARD_CHUNK);
    }

    #[test]
    /// The follow loop polls until told to stop, then ends the last line.
    fn print_follows_until_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "start\n").expect("write");
        let mut out = Vec::new();
        let mut rounds = 0;
        print(&path, 10, true, Duration::from_millis(1), &mut out, || {
            rounds += 1;
            if rounds == 2 {
                append(&path, "more\ntail");
            }
            rounds <= 3
        })
        .expect("print");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "start\nmore\ntail\n"
        );
    }

    #[cfg(unix)]
    #[test]
    /// Unix prints the bytes as written, invalid UTF-8 included.
    fn unix_passes_bytes_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, b"").expect("write");
        let mut out = Vec::new();
        let mut follower = Follower::start(&path, 10, &mut out).expect("start");
        append(&path, b"ok \xff\xc3");
        assert_eq!(polled_bytes(&mut follower), b"ok \xff\xc3");
        append(&path, b"\xa9\n");
        assert_eq!(polled_bytes(&mut follower), b"\xa9\n");
    }

    #[cfg(windows)]
    #[test]
    /// Windows replaces invalid UTF-8 for the console, and a character split
    /// between two reads prints whole once complete; an incomplete one left
    /// at the end is replaced.
    fn windows_prints_valid_utf8() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, b"").expect("write");
        let mut out = Vec::new();
        let mut follower = Follower::start(&path, 10, &mut out).expect("start");
        append(&path, b"ok \xff\xc3");
        assert_eq!(polled(&mut follower), "ok \u{fffd}");
        append(&path, b"\xa9\n");
        assert_eq!(polled(&mut follower), "\u{e9}\n");
        append(&path, b"cut \xe2\x82");
        assert_eq!(polled(&mut follower), "cut ");
        let mut out = Vec::new();
        follower.finish(&mut out).expect("finish");
        assert_eq!(String::from_utf8(out).expect("utf-8"), "\u{fffd}\n");
    }
}
