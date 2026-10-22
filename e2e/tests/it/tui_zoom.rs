//! The TUI's unit pane at every size: the real TUI runs in a pseudo-terminal
//! against the real daemon, and its screen is read back through vt100. Lines
//! printed before a zoom, an unzoom or a window resize must show re-wrapped at
//! the pane's new width and fill its rows. `devlanes attach` is driven the
//! same way.

use std::{
    io::{Read, Write},
    num::NonZeroU16,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use devlanes_e2e::{Error, Lab, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::BUDGET;

/// Size of the outer terminal the TUI starts in; zoomed, the pane is 200x45.
const COLUMNS: u16 = 203;
const ROWS: u16 = 50;
/// Width of the zoomed pane at `COLUMNS`.
const ZOOMED: usize = 200;
/// Outer terminal size after the window resize.
const RESIZED: (u16, u16) = (160, 40);
/// Length of every line the services print: wider than the unzoomed pane,
/// narrower than the zoomed one.
const LINE: usize = 180;
/// Lines a service prints at once before it slows down.
const BURST: usize = 150;
/// How long the pane may take to settle after a key or a resize.
const SETTLE: Duration = Duration::from_secs(6);

/// A service printing numbered `LINE`-character lines like a dev server: a
/// burst of `BURST` at startup, then one every 4 s, so nothing new arrives to
/// push old rows off the pane. Lines end in `END`.
fn service(name: &str, prefix: char, pty: bool) -> String {
    let fill = "abcdefghij"
        .repeat(LINE)
        .chars()
        .take(LINE - 10)
        .collect::<String>();
    format!(
        "  {name}:\n    run: 'i=0; while :; do i=$((i+1)); printf \"{prefix}%05d {fill}END\\n\" $i; \
         if [ $i -ge {BURST} ]; then sleep 4; fi; done'\n    pty: {pty}\n"
    )
}

/// vt100 sizes are nonzero.
fn cells(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).unwrap_or(NonZeroU16::MIN)
}

/// The real TUI in a pseudo-terminal, with its screen parsed as it draws.
struct Tui {
    screen: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Tui {
    /// Runs `devlanes ui` in the lab's repository at `COLUMNS`x`ROWS`.
    fn open(lab: &Lab) -> Result<Self> {
        Self::run(lab, &["ui"], (COLUMNS, ROWS))
    }

    /// Runs `devlanes <args>` in the lab's repository in a pseudo-terminal of
    /// `columns`x`rows`.
    fn run(lab: &Lab, args: &[&str], (columns, rows): (u16, u16)) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(setup)?;
        let template = lab.command(lab.repo(), args)?;
        let mut command = CommandBuilder::new(template.get_program());
        command.args(template.get_args());
        command.cwd(lab.repo());
        command.env_clear();
        for (key, value) in template.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).map_err(setup)?;
        drop(pair.slave);
        if let Some(pid) = child.process_id() {
            lab.track(pid.cast_signed());
        }
        let screen = Arc::new(Mutex::new(vt100::Parser::new(
            cells(rows),
            cells(columns),
            0,
        )));
        let mut reader = pair.master.try_clone_reader().map_err(setup)?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(setup)?));
        let (parsed, answer) = (screen.clone(), writer.clone());
        // Ends when the TUI exits and the pseudo-terminal closes.
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let chunk = &buffer[..read];
                let Ok(mut parser) = parsed.lock() else {
                    break;
                };
                parser.process(chunk);
                // crossterm asks for the cursor position at startup and fails
                // without an answer.
                if chunk.windows(4).any(|w| w == b"\x1b[6n") {
                    let (row, column) = parser.screen().cursor_position();
                    let reply = format!("\x1b[{};{}R", row + 1, column + 1);
                    if let Ok(mut writer) = answer.lock() {
                        let _ = writer.write_all(reply.as_bytes());
                        let _ = writer.flush();
                    }
                }
            }
        });
        Ok(Self {
            screen,
            writer,
            master: pair.master,
            child,
        })
    }

    /// Types `bytes` as if from the keyboard.
    fn keys(&mut self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resizes the window, as a user dragging it would.
    fn resize(&mut self, (columns, rows): (u16, u16)) -> Result<()> {
        self.screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .screen_mut()
            .set_size(cells(rows), cells(columns));
        self.master
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(setup)
    }

    /// The screen as text with row numbers, for failure messages.
    fn dump(&self) -> String {
        let parser = self.screen.lock().unwrap_or_else(|e| e.into_inner());
        let columns = parser.screen().size().1.get();
        parser
            .screen()
            .rows(0, columns)
            .enumerate()
            .map(|(i, row)| format!("{i:>2}|{}", row.trim_end()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The unit pane, the box at the right edge: its text width and the text
    /// of each row inside it, without trailing blanks.
    fn pane(&self) -> Option<(usize, Vec<String>)> {
        let parser = self.screen.lock().unwrap_or_else(|e| e.into_inner());
        let screen = parser.screen();
        let (rows, columns) = (screen.size().0.get(), screen.size().1.get());
        let at = |row: u16, column: u16| {
            screen
                .cell(row, column)
                .map(|cell| cell.contents().to_owned())
                .unwrap_or_default()
        };
        let right = columns - 1;
        let bottom = (2..rows).find(|&row| at(row, right) == "┘")?;
        let left = (0..right).rev().find(|&column| at(bottom, column) == "└")?;
        let top = (0..bottom).rev().find(|&row| at(row, left) == "┌")?;
        // A border and a blank column on the left, a border on the right.
        let width = right.checked_sub(left + 2)?;
        let text = screen
            .rows(left + 2, width)
            .skip(usize::from(top) + 1)
            .take(usize::from(bottom - top - 1))
            .map(|row| row.trim_end().to_owned())
            .collect();
        Some((usize::from(width), text))
    }

    /// Waits until the whole screen, `width` columns wide, holds the service's
    /// lines as [`filled_problem`] wants them. Otherwise says what is wrong,
    /// with the screen.
    fn screen_unfilled(&self, prefix: char, width: usize) -> Option<String> {
        let start = Instant::now();
        loop {
            let rows: Vec<String> = {
                let parser = self.screen.lock().unwrap_or_else(|e| e.into_inner());
                let columns = parser.screen().size().1.get();
                parser
                    .screen()
                    .rows(0, columns)
                    .map(|row| row.trim_end().to_owned())
                    .collect()
            };
            let problem = filled_problem(width, &rows, prefix)?;
            if start.elapsed() > SETTLE {
                return Some(format!("{problem}\n{}", self.dump()));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Waits for `text` anywhere on the screen.
    fn wait_for_text(&self, text: &str) -> Result<()> {
        let start = Instant::now();
        while !self.dump().contains(text) {
            if start.elapsed() > Duration::from_secs(20) {
                return Err(Error::Unexpected(format!(
                    "{text} never showed\n{}",
                    self.dump()
                )));
            }
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    /// Waits until the pane's width satisfies `width` and its rows hold the
    /// service's lines whole, each wrapped at that width, filling the pane
    /// from top to bottom. Otherwise says what is wrong, with the screen.
    fn unfilled(&self, stage: &str, prefix: char, width: impl Fn(usize) -> bool) -> Option<String> {
        let start = Instant::now();
        loop {
            let problem = match self.pane() {
                None => "no pane".to_owned(),
                Some((cols, _)) if !width(cols) => format!("pane is {cols} columns wide"),
                Some((cols, rows)) => filled_problem(cols, &rows, prefix)?,
            };
            if start.elapsed() > SETTLE {
                return Some(format!("{stage}: {problem}\n{}", self.dump()));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Tui {
    /// Kills the TUI; quitting it could ask whether to stop the lane.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Maps a pseudo-terminal failure into the harness error.
fn setup(error: impl std::fmt::Display) -> Error {
    Error::Setup(format!("pseudo-terminal: {error}"))
}

/// Whether `row` starts a service line: the prefix and a five-digit number.
fn starts_line(row: &str, prefix: char) -> bool {
    row.starts_with(prefix)
        && row
            .get(1..6)
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
}

/// What is wrong with a pane of `width` columns showing `rows`, if anything:
/// every line must be whole, take `LINE / width` rows rounded up with all but
/// its last row full, and the lines must fill the pane down to the row the
/// cursor waits on after the last one.
fn filled_problem(width: usize, rows: &[String], prefix: char) -> Option<String> {
    let filled = rows.iter().take_while(|row| !row.is_empty()).count();
    if filled + 1 < rows.len() {
        return Some(format!("only {filled} of {} rows filled", rows.len()));
    }
    let mut lines: Vec<Vec<&str>> = Vec::new();
    for row in &rows[..filled] {
        if starts_line(row, prefix) {
            lines.push(vec![row]);
        } else if let Some(line) = lines.last_mut() {
            line.push(row);
        }
        // Rows before the first line start belong to a line cut off at the top.
    }
    if lines.is_empty() {
        return Some("no lines".to_owned());
    }
    let per_line = LINE.div_ceil(width);
    lines.iter().find_map(|parts| {
        let joined = parts.concat();
        let whole = joined.len() == LINE && joined.ends_with("END");
        let wrapped =
            parts.len() == per_line && parts[..per_line - 1].iter().all(|p| p.len() == width);
        (!whole || !wrapped).then(|| {
            format!(
                "{} is not one {LINE}-column line wrapped at {width} columns: {:?}",
                &joined[..6.min(joined.len())],
                parts.iter().map(|p| p.len()).collect::<Vec<_>>()
            )
        })
    })
}

/// Selects `unit` with the arrow keys, waiting until its output shows.
fn select(tui: &mut Tui, unit: &str, prefix: char) -> Result<()> {
    let tag = format!("{prefix}{BURST:05}");
    for _ in 0..12 {
        if tui.dump().contains(&tag) {
            return Ok(());
        }
        tui.keys(b"\x1b[B")?;
        thread::sleep(Duration::from_millis(500));
    }
    Err(Error::Unexpected(format!(
        "{unit} never showed in the pane; screen:\n{}",
        tui.dump()
    )))
}

/// Watches `unit` in the pane, zooms, unzooms and resizes the window; after
/// each step the pane holds whole lines at its width and is filled.
fn pane_fills_at_every_size(pty: bool) -> Result<()> {
    let (unit, prefix) = if pty {
        ("ttyloop", 'P')
    } else {
        ("pipeloop", 'Q')
    };
    let yaml = format!(
        "project: fixture-zoom\nservices:\n{}",
        service(unit, prefix, pty)
    );
    let lab = Lab::with_config(&yaml, BUDGET)?;
    let started = lab.json(lab.repo(), &["start"])?;
    assert_eq!(started["ready"], true, "{started}");
    let mut tui = Tui::open(&lab)?;
    tui.wait_for_text(unit)?;
    select(&mut tui, unit, prefix)?;
    let (split, _) = tui
        .pane()
        .ok_or_else(|| Error::Unexpected(format!("no pane\n{}", tui.dump())))?;
    // Every step is checked, so a failure shows all that is wrong at once.
    let mut problems = Vec::new();
    problems.extend(tui.unfilled("watching", prefix, |width| width < LINE));

    tui.keys(b"z")?;
    problems.extend(tui.unfilled("zoomed", prefix, |width| width == ZOOMED));

    tui.keys(b"z")?;
    problems.extend(tui.unfilled("unzoomed", prefix, |width| width == split));

    tui.resize(RESIZED)?;
    problems.extend(tui.unfilled("window resized", prefix, |width| width < split));
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::Unexpected(problems.join("\n\n")))
    }
}

/// A PTY unit: existing output is re-wrapped at every pane size.
#[test]
fn tui_pane_fills_at_every_size_for_a_pty_unit() -> Result<()> {
    pane_fills_at_every_size(true)
}

/// A pipe unit has no program to redraw; its lines are re-wrapped all the same.
#[test]
fn tui_pane_fills_at_every_size_for_a_pipe_unit() -> Result<()> {
    pane_fills_at_every_size(false)
}

/// `devlanes attach` shows a PTY unit's earlier output re-wrapped at the
/// terminal's size, and lines typed into it reach the unit. The terminal is
/// larger than the unit's default 120x40, so the earlier lines must fill rows
/// the unit never drew.
#[test]
fn attach_draws_at_the_terminal_size_and_takes_input() -> Result<()> {
    const SIZE: (u16, u16) = (200, 45);
    let yaml = format!(
        "project: fixture-zoom\nservices:\n{}  echo:\n    run: 'while read line; do \
         printf \"got %s\\n\" \"$line\"; done'\n    pty: true\n",
        service("ttyloop", 'P', true)
    );
    let lab = Lab::with_config(&yaml, BUDGET)?;
    let started = lab.json(lab.repo(), &["start"])?;
    assert_eq!(started["ready"], true, "{started}");
    {
        let attached = Tui::run(&lab, &["attach", "ttyloop"], SIZE)?;
        attached.wait_for_text(&format!("P{BURST:05}"))?;
        if let Some(problem) = attached.screen_unfilled('P', usize::from(SIZE.0)) {
            return Err(Error::Unexpected(format!("attached: {problem}")));
        }
    }
    let mut attached = Tui::run(&lab, &["attach", "echo"], SIZE)?;
    // Keys typed before the stream opens wait in the terminal for the client.
    attached.keys(b"acme\r")?;
    attached.wait_for_text("got acme")
}
