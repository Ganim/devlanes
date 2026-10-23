//! Windows only: the CLI under Git Bash in a console. Git Bash starts native
//! programs with the inherited "ignore Ctrl-C" flag set and delivers Ctrl-C
//! to its foreground program itself, so like a Unix shell's job control,
//! Ctrl-C reaches the command in the foreground and spares background jobs.

use std::{
    io::{Read, Write},
    num::NonZeroU16,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use devlanes_e2e::{Error, Lab, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::BUDGET;

/// A service that prints a line every second, for `logs -f` to follow.
const CONFIG: &str = "\
project: fixture-bash
services:
  web:
    run: 'while :; do echo tick; sleep 1; done'
";

/// Console size; wide enough that no line wraps.
const SIZE: (u16, u16) = (160, 40);
/// How long a line may take to show.
const WAIT: Duration = Duration::from_secs(30);

/// An interactive Git Bash in a pseudo console, with its screen parsed as it
/// draws.
struct Bash {
    screen: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Bash {
    /// Starts `bash -i` in the lab's repository with the lab's environment
    /// and human output, as a terminal would start it: without the ignore flag.
    fn open(lab: &Lab) -> Result<Self> {
        let program = git_bash()?;
        // The test runner may have set the flag, which bash would inherit.
        devlanes_sys::windows::process::accept_ctrl_c()?;
        let (columns, rows) = SIZE;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(setup)?;
        let template = lab.command(lab.repo(), &[])?;
        let mut command = CommandBuilder::new(program);
        command.args(["--norc", "--noprofile", "-i"]);
        command.cwd(lab.repo());
        command.env_clear();
        for (key, value) in template.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        command.env("DEVLANES_OUTPUT", "human");
        command.env("PS1", "$ ");
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).map_err(setup)?;
        drop(pair.slave);
        let screen = Arc::new(Mutex::new(vt100::Parser::new(
            cells(rows),
            cells(columns),
            0,
        )));
        let mut reader = pair.master.try_clone_reader().map_err(setup)?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(setup)?));
        let (parsed, answer) = (screen.clone(), writer.clone());
        // Ends when bash exits and the pseudo console closes.
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let chunk = &buffer[..read];
                if let Ok(mut parser) = parsed.lock() {
                    parser.process(chunk);
                }
                // The pseudo console asks for the cursor position at startup
                // and waits for the answer.
                if chunk.windows(4).any(|w| w == b"\x1b[6n")
                    && let Ok(mut writer) = answer.lock()
                {
                    let _ = writer.write_all(b"\x1b[1;1R");
                    let _ = writer.flush();
                }
            }
        });
        Ok(Self {
            screen,
            writer,
            _master: pair.master,
            child,
        })
    }

    /// Types `bytes` as if from the keyboard.
    fn keys(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// The screen as text, without trailing blanks.
    fn text(&self) -> String {
        let parser = self.screen.lock().unwrap_or_else(|e| e.into_inner());
        parser
            .screen()
            .rows(0, SIZE.0)
            .map(|row| row.trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Waits until a row of the screen starts with `line`.
    fn wait_for(&self, line: &str) -> Result<()> {
        let start = Instant::now();
        loop {
            let text = self.text();
            if text.lines().any(|row| row.starts_with(line)) {
                return Ok(());
            }
            if start.elapsed() > WAIT {
                return Err(Error::Unexpected(format!("{line:?} never showed:\n{text}")));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Bash {
    /// Ends bash; the lab stops what its commands started.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Git for Windows' `bash.exe`, next to the `git.exe` on `PATH`
/// (`<Git>\cmd\git.exe` and `<Git>\bin\bash.exe`).
fn git_bash() -> Result<PathBuf> {
    let output = std::process::Command::new("where.exe")
        .arg("git")
        .output()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|git| {
            PathBuf::from(git.trim())
                .parent()?
                .parent()
                .map(PathBuf::from)
        })
        .map(|root| root.join(r"bin\bash.exe"))
        .find(|bash| bash.is_file())
        .ok_or_else(|| Error::Setup("Git for Windows' bash.exe is not installed".into()))
}

/// vt100 sizes are nonzero.
fn cells(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).unwrap_or(NonZeroU16::MIN)
}

/// Maps a pseudo-terminal failure into the harness error.
fn setup(error: impl std::fmt::Display) -> Error {
    Error::Setup(format!("pseudo-terminal: {error}"))
}

/// Ctrl-C detaches `logs -f` in the foreground, which says so, and leaves the
/// same command running as a background job.
#[test]
fn ctrl_c_reaches_the_foreground_command_and_spares_background_jobs() -> Result<()> {
    let lab = Lab::with_config(CONFIG, BUDGET)?;
    let started = lab.json(lab.repo(), &["start"])?;
    assert_eq!(started["ready"], true, "{started}");
    let bash = Bash::open(&lab)?;
    bash.keys(b"devlanes logs -f web > background.log 2>&1 &\r")?;
    bash.wait_for("[1] ")?;
    bash.keys(b"devlanes logs -f web\r")?;
    bash.wait_for("web#")?;
    bash.keys(b"\x03")?;
    bash.wait_for("detached; services keep running")?;
    // Long enough for a Ctrl-C that reached the job to have ended it.
    thread::sleep(Duration::from_secs(2));
    bash.keys(b"echo \"running jobs: $(jobs -r | wc -l)\"\r")?;
    bash.wait_for("running jobs: ")?;
    let text = bash.text();
    assert!(text.contains("running jobs: 1"), "{text}");
    bash.keys(b"kill %1\r")?;
    Ok(())
}
