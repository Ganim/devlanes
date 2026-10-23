//! `devlanes daemon logs` on Windows, which has no `tail`: the last lines of
//! `daemon.log`, then with `-f` what is appended, across the rotation a
//! daemon start does (`daemon.log` renamed to `daemon.log.1`, a new one
//! created) and truncation.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use devlanes_sys_windows::file::{FileId, file_identity, path_identity};

/// How often `-f` looks for appended bytes and a replaced file.
const POLL: Duration = Duration::from_millis(250);
/// The one read buffer; reads never allocate past it.
const BUFFER: usize = 64 * 1024;

/// Prints the last `lines` lines of `path` to `out` and, with `follow`, keeps
/// printing what is appended until the process is stopped. Partial lines are
/// printed as they arrive. Without `follow` a missing file is an error; with
/// it, the follower waits for the file to appear. Blocking.
pub fn tail(path: &Path, lines: usize, follow: bool, out: &mut impl Write) -> io::Result<()> {
    follow_while(path, lines, follow, out, POLL, &|| true)
}

/// [`tail`] with the poll interval given and polling only while `running`
/// says so, so tests can end it.
fn follow_while(
    path: &Path,
    lines: usize,
    follow: bool,
    out: &mut impl Write,
    poll: Duration,
    running: &dyn Fn() -> bool,
) -> io::Result<()> {
    let mut buffer = vec![0; BUFFER];
    let mut current = match File::open(path) {
        Ok(file) => Some(file),
        Err(error) if follow && error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(file) = &mut current {
        let start = last_lines_start(file, lines, &mut buffer)?;
        file.seek(SeekFrom::Start(start))?;
        copy_available(file, out, &mut buffer)?;
    }
    out.flush()?;
    if !follow {
        return Ok(());
    }
    let mut identity = current.as_ref().and_then(|file| file_identity(file).ok());
    while running() {
        std::thread::sleep(poll);
        if let Some(file) = &mut current {
            copy_available(file, out, &mut buffer)?;
            // Truncated in place: start over from its beginning.
            if file.metadata()?.len() < file.stream_position()? {
                file.seek(SeekFrom::Start(0))?;
                copy_available(file, out, &mut buffer)?;
            }
        }
        // Another file under the name means the log was rotated; between the
        // rename and the new file there may be none, which is not an error.
        let named: Option<FileId> = path_identity(path).ok();
        if named.is_some()
            && named != identity
            && let Ok(mut file) = File::open(path)
        {
            // What the old file got after the last poll comes first.
            if let Some(old) = &mut current {
                copy_available(old, out, &mut buffer)?;
            }
            identity = file_identity(&file).ok();
            copy_available(&mut file, out, &mut buffer)?;
            current = Some(file);
        }
        out.flush()?;
    }
    Ok(())
}

/// Copies everything from `file`'s position to its current end into `out`.
fn copy_available(file: &mut File, out: &mut impl Write, buffer: &mut [u8]) -> io::Result<()> {
    loop {
        let read = file.read(buffer)?;
        if read == 0 {
            return Ok(());
        }
        out.write_all(&buffer[..read])?;
    }
}

/// The offset where the last `lines` lines of `file` start, reading backwards
/// through `buffer`. A newline at the very end ends the last line instead of
/// starting an empty one, as with `tail`.
fn last_lines_start(file: &mut File, lines: usize, buffer: &mut [u8]) -> io::Result<u64> {
    let len = file.metadata()?.len();
    if lines == 0 || len == 0 {
        return Ok(len);
    }
    let mut end = len;
    let mut seen = 0;
    let mut last_byte = true;
    while end > 0 {
        let size = buffer.len().min(usize::try_from(end).unwrap_or(usize::MAX));
        let start = end - size as u64;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..size])?;
        for (index, byte) in buffer[..size].iter().enumerate().rev() {
            if *byte == b'\n' && !last_byte {
                seen += 1;
                if seen == lines {
                    return Ok(start + index as u64 + 1);
                }
            }
            last_byte = false;
        }
        end = start;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Instant;

    use super::*;

    /// Output shared with the follower's thread.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Shared {
        /// Everything written so far, as text.
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
                .into_owned()
        }

        /// Waits up to 10 s for the output to end with `suffix`.
        fn wait_for(&self, suffix: &str) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !self.text().ends_with(suffix) {
                assert!(
                    Instant::now() < deadline,
                    "never got {suffix:?}: {:?}",
                    self.text()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Appends `text` to `path`.
    fn append(path: &Path, text: &str) {
        let mut file = File::options()
            .append(true)
            .create(true)
            .open(path)
            .expect("open");
        file.write_all(text.as_bytes()).expect("append");
    }

    #[test]
    /// The last lines come out whole; a final line without a newline counts,
    /// and a trailing newline does not start an extra one.
    fn last_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        let text: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&path, &text).expect("write");
        let read = |lines| {
            let mut out = Vec::new();
            tail(&path, lines, false, &mut out).expect("tail");
            String::from_utf8(out).expect("text")
        };
        assert_eq!(read(3), "line 8\nline 9\nline 10\n");
        assert_eq!(read(0), "");
        assert_eq!(read(50), text);
        append(&path, "partial");
        assert_eq!(read(2), "line 10\npartial");
        // Lines longer than the read buffer are found across reads.
        let long = "x".repeat(BUFFER * 2);
        std::fs::write(&path, format!("a\n{long}\nb\n")).expect("write");
        assert_eq!(read(2), format!("{long}\nb\n"));
        let missing = tail(&dir.path().join("gone.log"), 5, false, &mut Vec::new());
        assert_eq!(
            missing.map_err(|error| error.kind()),
            Err(io::ErrorKind::NotFound)
        );
    }

    #[test]
    /// `-f` prints appends as they come, partial lines included, then follows
    /// the name to a new file after a rotation and starts it over after a
    /// truncation.
    fn follows_appends_rotation_and_truncation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, "old 1\nold 2\n").expect("write");
        let out = Shared::default();
        let running = Arc::new(AtomicBool::new(true));
        let follower = {
            let (path, mut out, running) = (path.clone(), out.clone(), running.clone());
            std::thread::spawn(move || {
                follow_while(&path, 1, true, &mut out, Duration::from_millis(20), &|| {
                    running.load(Ordering::SeqCst)
                })
            })
        };
        out.wait_for("old 2\n");
        append(&path, "new 1\npart");
        out.wait_for("new 1\npart");
        append(&path, "ial\n");
        out.wait_for("partial\n");
        std::fs::rename(&path, dir.path().join("daemon.log.1")).expect("rotate");
        std::fs::write(&path, "fresh 1\n").expect("new file");
        out.wait_for("fresh 1\n");
        append(&path, "fresh 2\n");
        out.wait_for("fresh 2\n");
        std::fs::write(&path, "cut\n").expect("truncate");
        out.wait_for("cut\n");
        running.store(false, Ordering::SeqCst);
        follower.join().expect("join").expect("follow");
        assert_eq!(out.text(), "old 2\nnew 1\npartial\nfresh 1\nfresh 2\ncut\n");
    }

    #[test]
    /// `-f` on a file that does not exist yet waits for it.
    fn follow_waits_for_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.log");
        let out = Shared::default();
        let running = Arc::new(AtomicBool::new(true));
        let follower = {
            let (path, mut out, running) = (path.clone(), out.clone(), running.clone());
            std::thread::spawn(move || {
                follow_while(&path, 5, true, &mut out, Duration::from_millis(20), &|| {
                    running.load(Ordering::SeqCst)
                })
            })
        };
        std::fs::write(&path, "first\n").expect("write");
        out.wait_for("first\n");
        running.store(false, Ordering::SeqCst);
        follower.join().expect("join").expect("follow");
    }
}
