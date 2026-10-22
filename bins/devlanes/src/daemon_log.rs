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

/// Upper bound per forward read, so a burst of logging is printed in pieces.
const FORWARD_CHUNK: usize = 1024 * 1024;

/// The log file being printed: which file, how far, and an unfinished last line.
pub struct Follower {
    path: PathBuf,
    /// The open file; `None` until the path exists.
    file: Option<File>,
    identity: FileIdentity,
    offset: u64,
    /// The end of the file after its last newline, held until the line is complete.
    partial: Vec<u8>,
}

impl Follower {
    /// Opens `path` and prints its last `lines` lines to `out`; an unfinished
    /// last line is held back until [`poll`](Self::poll) sees it complete or
    /// [`finish`](Self::finish) prints it. A missing file prints nothing and is
    /// picked up once it appears.
    pub fn start(path: &Path, lines: usize, out: &mut impl Write) -> io::Result<Self> {
        let mut follower = Self {
            path: path.to_owned(),
            file: None,
            identity: FileIdentity::default(),
            offset: 0,
            partial: Vec::new(),
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
    /// (rotation, or a log that only now appeared), the rest of the old one is
    /// printed first and the new one from its start.
    pub fn poll(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.file.is_some() {
            self.read_to_end(out)?;
        }
        let replaced = match devlanes_sys::path_identity(&self.path) {
            Ok(identity) => self.file.is_none() || identity != self.identity,
            // Between a rotation's rename and the new file: keep the old one.
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        if !replaced {
            return Ok(());
        }
        let Some(file) = open(&self.path)? else {
            return Ok(());
        };
        self.finish(out)?;
        self.identity = devlanes_sys::file_identity(&file)?;
        self.file = Some(file);
        self.offset = 0;
        self.read_to_end(out)
    }

    /// Prints the held-back unfinished line, if any, ending it with a newline.
    pub fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        if !self.partial.is_empty() {
            self.partial.push(b'\n');
            write_text(out, &self.partial)?;
            self.partial.clear();
        }
        out.flush()
    }

    /// Prints complete lines from the offset to the current end of the file.
    fn read_to_end(&mut self, out: &mut impl Write) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let len = file.metadata()?.len();
        if len < self.offset {
            // Truncated in place: whatever was held belongs to the old content.
            self.offset = 0;
            self.partial.clear();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buffer = vec![0; FORWARD_CHUNK];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            self.offset += u64::try_from(read).unwrap_or(u64::MAX);
            self.partial.extend_from_slice(&buffer[..read]);
            if let Some(end) = self.partial.iter().rposition(|byte| *byte == b'\n') {
                let rest = self.partial.split_off(end + 1);
                write_text(out, &self.partial)?;
                self.partial = rest;
            }
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

/// Writes `bytes` as UTF-8, replacing invalid sequences: Windows consoles reject
/// writes that are not valid UTF-8.
fn write_text(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    out.write_all(String::from_utf8_lossy(bytes).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `follower.poll` prints.
    fn polled(follower: &mut Follower) -> String {
        let mut out = Vec::new();
        follower.poll(&mut out).expect("poll");
        String::from_utf8(out).expect("utf-8")
    }

    /// Appends `text` to `path`.
    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .expect("open");
        file.write_all(text.as_bytes()).expect("append");
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
    /// Appended lines print once complete; a truncated file starts over.
    fn follows_appends_and_truncation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "old\n").expect("write");
        let mut out = Vec::new();
        let mut follower = Follower::start(&path, 10, &mut out).expect("start");
        assert_eq!(String::from_utf8(out).expect("utf-8"), "old\n");
        assert_eq!(polled(&mut follower), "");
        append(&path, "new\npar");
        assert_eq!(polled(&mut follower), "new\n");
        append(&path, "tial\n");
        assert_eq!(polled(&mut follower), "partial\n");
        std::fs::write(&path, "fresh\n").expect("truncate");
        assert_eq!(polled(&mut follower), "fresh\n");
    }

    #[test]
    /// A rotation prints the rest of the old file, then the new one from its
    /// start, even while the follower holds the old file open.
    fn follows_rotation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "a\n").expect("write");
        let mut out = Vec::new();
        let mut follower = Follower::start(&path, 10, &mut out).expect("start");
        append(&path, "b\nunfinished");
        // The daemon renames the log while this reader has it open, as it does
        // at startup, so the rename must work on Windows too.
        devlanes_sys::rename_retrying(&path, &dir.path().join("daemon.log.1")).expect("rotate");
        // Between the rename and the new file the old one is still read.
        assert_eq!(polled(&mut follower), "b\n");
        std::fs::write(&path, "c\n").expect("new log");
        assert_eq!(polled(&mut follower), "unfinished\nc\n");
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
    /// The follow loop polls until told to stop, then prints a held line.
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
}
