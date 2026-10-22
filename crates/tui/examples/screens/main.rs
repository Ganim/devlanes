//! Renders every TUI page and interaction state against a fake daemon, for
//! design review: `cargo run -p devlanes-tui --example screens`.
//!
//! Each state is written at several terminal sizes as a PNG (real colors), a
//! plain-text file and an ANSI dump (`cat` it in a true-color terminal), into
//! `target/tui-screens/` with an `index.md` listing them. A first argument
//! replaces the output directory; `DEVLANES_SCREENS_FONT` names a font file to
//! draw with first.

mod data;
mod paint;

use std::{collections::VecDeque, fmt::Write as _, fs, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use devlanes_client::update::InstallMethod;
use devlanes_proto::{AttachResult, Event, Page, RpcError, events};
use devlanes_tui::{
    ApiError,
    app::{App, Effect, Msg, RowId, Section, Selected, Target},
    logfile::{LogChunk, LogPosition},
    start::StartFrame,
    theme::{Glyphs, Theme, Tier},
    ui,
};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
use serde_json::json;

/// Terminal sizes every state is drawn at.
const SIZES: [(u16, u16); 3] = [(200, 60), (120, 40), (80, 24)];
/// Messages one step may cause before the fake daemon stops answering.
const STEP_BUDGET: usize = 2_000;

/// The TUI with a fake daemon that answers every effect at once.
struct Fake {
    app: App,
    queue: VecDeque<Msg>,
}

impl Fake {
    /// A TUI of `width` x `height` that subscribed and loaded the lanes.
    fn new(width: u16, height: u16) -> Self {
        let mut app = App::new(width, height);
        // Opened from the monorepo's main worktree.
        app.prefer_path("/work/acme".into());
        let mut fake = Self {
            app,
            queue: VecDeque::new(),
        };
        for effect in fake.app.start() {
            fake.answer(effect);
        }
        fake.drain();
        fake
    }

    /// Applies `msg` and everything the fake daemon answers to its effects.
    fn send(&mut self, msg: Msg) {
        self.queue.push_back(msg);
        self.drain();
    }

    /// Presses `code`.
    fn key(&mut self, code: KeyCode) {
        self.send(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    /// Presses Shift and `c`.
    fn shifted(&mut self, c: char) {
        self.send(Msg::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::SHIFT,
        )));
    }

    /// Selects `row` of lane `lane`, unfolding what hides it.
    fn select(&mut self, lane: &str, row: RowId) {
        let mut effects = Vec::new();
        self.app.select(
            Selected {
                lane: lane.into(),
                row,
            },
            &mut effects,
        );
        for effect in effects {
            self.answer(effect);
        }
        self.drain();
    }

    /// Selects the unit `name` of lane `lane`.
    fn unit(&mut self, lane: &str, name: &str) {
        self.select(lane, RowId::Unit(name.into()));
    }

    /// Selects the whole-lane row of `lane`.
    fn lane(&mut self, lane: &str) {
        self.select(lane, RowId::WholeLane);
    }

    /// Types `text` one key at a time.
    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    /// Runs queued messages until none are left.
    fn drain(&mut self) {
        for _ in 0..STEP_BUDGET {
            let Some(msg) = self.queue.pop_front() else {
                return;
            };
            for effect in self.app.update(msg) {
                self.answer(effect);
            }
        }
    }

    /// What the fake daemon sends back for one effect.
    fn answer(&mut self, effect: Effect) {
        let unit_name = |reference: &str| {
            reference
                .rsplit_once(':')
                .map_or(reference, |(_, unit)| unit)
                .to_owned()
        };
        let reply = match effect {
            Effect::Subscribe => vec![Msg::Subscribed(Ok(()))],
            Effect::Refresh => vec![Msg::Status(Ok(data::lanes()))],
            Effect::Attach {
                generation,
                lane,
                unit,
                ..
            } => vec![
                Msg::AttachOpened {
                    generation,
                    result: Ok(AttachResult {
                        stream_id: 1,
                        unit: unit.clone(),
                        lane,
                    }),
                },
                Msg::AttachOutput {
                    generation,
                    bytes: data::screen(&unit_name(&unit)),
                },
            ],
            Effect::FetchLogs {
                generation,
                query,
                older,
                ..
            } => {
                let unit = query
                    .units
                    .first()
                    .map(|u| unit_name(u))
                    .unwrap_or_default();
                vec![Msg::Logs {
                    generation,
                    older,
                    result: Ok(Page::complete(data::log_page(&unit))),
                }]
            }
            Effect::FetchMetrics(_) => vec![Msg::Metrics(Ok(data::metrics()))],
            Effect::FetchDaemon => vec![Msg::DaemonInfo {
                status: Ok(data::daemon_status()),
                doctor: Ok(data::doctor()),
            }],
            Effect::ReadDaemonLog { .. } => vec![Msg::DaemonLog(Ok(LogChunk {
                lines: data::daemon_log(),
                position: LogPosition {
                    offset: 4_096,
                    file: (1, 1),
                },
                reset: true,
            }))],
            Effect::FetchProfiles { lane } => vec![Msg::Profiles {
                result: Ok(data::profiles(&lane)),
                lane,
            }],
            Effect::FetchTasks { lane } => vec![Msg::Tasks {
                result: Ok(data::tasks(&lane)),
                lane,
            }],
            _ => Vec::new(),
        };
        self.queue.extend(reply);
    }
}

/// One state to draw, and how to get there.
struct Scene {
    name: &'static str,
    what: &'static str,
    setup: fn(&mut Fake),
}

/// A `lane.phase` or `task` event of `acme@1`'s start.
fn start_event(name: &str, data: serde_json::Value) -> Msg {
    Msg::Start(StartFrame::Event(Event {
        seq: 1,
        event: name.into(),
        data,
    }))
}

/// The scene `lanes-unit` in another color tier and glyph set.
fn themed(f: &mut Fake, tier: Tier, glyphs: Glyphs) {
    f.app.theme = Theme { tier, glyphs };
    f.unit("acme@0", "api-gateway");
}

/// Every state, in review order.
fn scenes() -> Vec<Scene> {
    vec![
        Scene {
            name: "lanes-unit",
            what: "project view, watching a running service: tabs, lane chips, sections and groups",
            setup: |f| f.unit("acme@0", "api-gateway"),
        },
        Scene {
            name: "lanes-typing",
            what: "the same service while typing into it (Enter): the footer banner says how back",
            setup: |f| {
                f.unit("acme@0", "api-gateway");
                f.key(KeyCode::Enter);
            },
        },
        Scene {
            name: "lanes-zoomed",
            what: "the pane zoomed (z)",
            setup: |f| {
                f.unit("acme@0", "wrk-index");
                f.key(KeyCode::Char('z'));
            },
        },
        Scene {
            name: "sections-folded",
            what: "tasks and infra folded by the user (h), the whole-lane row selected",
            setup: |f| {
                f.select("acme@0", RowId::Section(Section::Tasks));
                f.key(KeyCode::Char('h'));
                f.select("acme@0", RowId::Section(Section::Infra));
                f.key(KeyCode::Char('h'));
                f.key(KeyCode::Char('g'));
            },
        },
        Scene {
            name: "section-selected",
            what: "the services section selected: its units with state, url, cpu and memory",
            setup: |f| f.select("acme@0", RowId::Section(Section::Services)),
        },
        Scene {
            name: "not-started-open",
            what: "the not started fold opened (l): services that are not running while the lane runs",
            setup: |f| {
                f.select(
                    "acme@0",
                    RowId::Group(Section::Services, "not started".into()),
                );
                f.key(KeyCode::Char('l'));
            },
        },
        Scene {
            name: "setup-open",
            what: "the infra section with the setup group unfolded",
            setup: |f| {
                f.select("acme@0", RowId::Group(Section::Infra, "setup".into()));
                f.key(KeyCode::Char('l'));
            },
        },
        Scene {
            name: "lane-overview",
            what: "the whole-lane row: facts, services not up, and every lane of the project",
            setup: |f| f.lane("acme@0"),
        },
        Scene {
            name: "lane-starting",
            what: "a lane in the middle of its start: phases on the whole-lane row, ] to reach it",
            setup: |f| {
                for data in data::start_phases() {
                    f.send(Msg::Event(Event {
                        seq: 1,
                        event: events::PHASE.into(),
                        data,
                    }));
                }
                f.key(KeyCode::Char(']'));
            },
        },
        Scene {
            name: "lane-stopped",
            what: "a lane with nothing running: every service listed, s starts it",
            setup: |f| f.lane("acme@2"),
        },
        Scene {
            name: "lane-missing-worktree",
            what: "a lane whose worktree directory is gone",
            setup: |f| f.lane("acme@3"),
        },
        Scene {
            name: "project-shop",
            what: "another project tab (3)",
            setup: |f| f.key(KeyCode::Char('3')),
        },
        Scene {
            name: "project-config-error",
            what: "a project whose devlanes.yaml does not load (2)",
            setup: |f| f.key(KeyCode::Char('2')),
        },
        Scene {
            name: "unit-failed",
            what: "a failed service: failure card above its last output",
            setup: |f| f.unit("acme@0", "wrk-export"),
        },
        Scene {
            name: "unit-orphan",
            what: "a process an earlier daemon left running",
            setup: |f| f.unit("acme@0", "stub-twilio"),
        },
        Scene {
            name: "unit-stopped",
            what: "a service that is not running (autostart off); hidden services are dimmed",
            setup: |f| f.unit("acme@0", "web-storybook"),
        },
        Scene {
            name: "task-never-run",
            what: "a task from task.list that never ran",
            setup: |f| f.select("acme@0", RowId::Task("lint".into())),
        },
        Scene {
            name: "unit-logs",
            what: "stored logs of a unit after scrolling past its screen (PgUp)",
            setup: |f| {
                f.unit("acme@0", "api-orders");
                f.key(KeyCode::PageUp);
                f.key(KeyCode::PageUp);
            },
        },
        Scene {
            name: "filter",
            what: "the list filtered with /wrk while typing the filter",
            setup: |f| {
                f.unit("acme@0", "api-gateway");
                f.key(KeyCode::Char('/'));
                f.type_text("wrk");
            },
        },
        Scene {
            name: "start-in-progress",
            what: "devlanes start running inside the TUI: phase in the top bar",
            setup: |f| {
                f.app.open_start("start".into());
                f.send(start_event(
                    events::PHASE,
                    json!({"lane": "acme@1", "phase": "tasks", "state": "started", "start_id": 7}),
                ));
                f.send(start_event(
                    events::TASK,
                    json!({"lane": "acme@1", "task": "migrate", "state": "started"}),
                ));
            },
        },
        Scene {
            name: "dialog-clean",
            what: "clean confirmation (C)",
            setup: |f| {
                f.lane("acme@2");
                f.shifted('C');
            },
        },
        Scene {
            name: "dialog-profiles",
            what: "profile picker (p)",
            setup: |f| {
                f.lane("acme@0");
                f.key(KeyCode::Char('p'));
            },
        },
        Scene {
            name: "dialog-run-task",
            what: "a task whose config asks first (s on seed)",
            setup: |f| {
                f.unit("acme@0", "seed");
                f.key(KeyCode::Char('s'));
            },
        },
        Scene {
            name: "dialog-help",
            what: "every key (?), this screen's first",
            setup: |f| f.key(KeyCode::Char('?')),
        },
        Scene {
            name: "first-run",
            what: "the first-run hint strip above the footer",
            setup: |f| {
                f.send(Msg::Notices {
                    method: InstallMethod::Homebrew,
                    dismissed: Vec::new(),
                });
                f.unit("acme@0", "api-gateway");
            },
        },
        Scene {
            name: "theme-dark",
            what: "true color on a dark background",
            setup: |f| themed(f, Tier::Dark, Glyphs::Unicode),
        },
        Scene {
            name: "theme-light",
            what: "true color on a light background",
            setup: |f| themed(f, Tier::Light, Glyphs::Unicode),
        },
        Scene {
            name: "theme-no-color",
            what: "NO_COLOR: shapes, words, bold and reverse video",
            setup: |f| themed(f, Tier::Plain, Glyphs::Unicode),
        },
        Scene {
            name: "glyphs-ascii",
            what: "ASCII glyphs (ui.glyphs: ascii, or a locale without UTF-8)",
            setup: |f| themed(f, Tier::Ansi, Glyphs::Ascii),
        },
        Scene {
            name: "monitor",
            what: "the monitor (M): grouped by lane, sorted by cpu, api-search selected",
            setup: |f| {
                f.shifted('M');
                f.send(Msg::Tick);
                f.key(KeyCode::Char('j'));
            },
        },
        Scene {
            name: "monitor-sorted-filtered",
            what: "flat (e), sorted by memory (>), filtered with /acme",
            setup: |f| {
                f.shifted('M');
                f.key(KeyCode::Char('e'));
                f.key(KeyCode::Char('>'));
                f.key(KeyCode::Char('/'));
                f.type_text("acme");
                f.key(KeyCode::Enter);
                f.key(KeyCode::Char('j'));
                f.key(KeyCode::Char('j'));
                f.key(KeyCode::Char('j'));
            },
        },
        Scene {
            name: "monitor-detail",
            what: "the detail graphs zoomed (z) on api-search",
            setup: |f| {
                f.shifted('M');
                f.key(KeyCode::Char('j'));
                f.key(KeyCode::Char('z'));
            },
        },
        Scene {
            name: "daemon",
            what: "the daemon page (D): facts, doctor checks and the daemon log",
            setup: |f| f.shifted('D'),
        },
        Scene {
            name: "daemon-copy",
            what: "copy mode on the daemon log (c, v, k): two lines selected",
            setup: |f| {
                f.shifted('D');
                f.key(KeyCode::Char('c'));
                f.key(KeyCode::Char('k'));
                f.key(KeyCode::Char('v'));
                f.key(KeyCode::Char('k'));
            },
        },
        Scene {
            name: "unit-start-failed",
            what: "a start that failed: badge on the row, full error in the pane",
            setup: |f| {
                f.unit("acme@0", "wrk-export");
                f.send(Msg::ActionDone {
                    label: "start acme@0:wrk-export".into(),
                    target: Target::Unit {
                        lane: "acme@0".into(),
                        unit: "wrk-export".into(),
                    },
                    result: Err(ApiError::Rpc(RpcError::new(
                        "compose_failed",
                        json!({"message": "`docker compose -p acme up -d --wait` failed (exit status: 1): container acme-postgres-1 is unhealthy\nlast lines of its log follow"}),
                        1,
                    ))),
                });
            },
        },
        Scene {
            name: "update-badge",
            what: "a newer release in the header",
            setup: |f| {
                let mut status = data::daemon_status();
                status["update"] = json!({"latest": "0.2.0", "checked_at": 1});
                f.send(Msg::DaemonInfo {
                    status: Ok(status),
                    doctor: Ok(data::doctor()),
                });
                f.send(Msg::Notices {
                    method: InstallMethod::Homebrew,
                    dismissed: Vec::new(),
                });
                f.unit("acme@0", "web-app");
            },
        },
    ]
}

/// Draws the app into a buffer of its size.
fn render(app: &App) -> Result<Buffer, Box<dyn std::error::Error>> {
    let mut terminal = Terminal::new(TestBackend::new(app.size.0, app.size.1))?;
    terminal.draw(|frame| ui::render(app, frame))?;
    Ok(terminal.backend().buffer().clone())
}

/// Writes the three files of one state and returns their base name.
fn write(
    out: &std::path::Path,
    fonts: Option<&paint::Fonts>,
    name: &str,
    app: &App,
) -> Result<String, Box<dyn std::error::Error>> {
    let base = format!("{name}-{}x{}", app.size.0, app.size.1);
    let buffer = render(app)?;
    fs::write(out.join(format!("{base}.txt")), paint::text(&buffer))?;
    fs::write(out.join(format!("{base}.ansi")), paint::ansi(&buffer))?;
    if let Some(fonts) = fonts {
        let light = app.theme.tier == Tier::Light;
        paint::png(&buffer, fonts, &out.join(format!("{base}.png")), light)?;
    }
    Ok(base)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args().nth(1).map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/tui-screens"),
        PathBuf::from,
    );
    fs::create_dir_all(&out)?;
    let fonts = paint::Fonts::load();
    if fonts.is_none() {
        eprintln!(
            "no monospace font found; writing text and ANSI only (set DEVLANES_SCREENS_FONT)"
        );
    }
    let mut index = String::from(
        "# devlanes TUI screens\n\nGenerated by `cargo run -p devlanes-tui --example screens` against a fake daemon. Each state has a PNG, the plain text and an ANSI dump (`cat` it in a true-color terminal).\n\n",
    );
    for scene in scenes() {
        let _ = writeln!(index, "## {}\n\n{}\n", scene.name, scene.what);
        for (width, height) in SIZES {
            let mut fake = Fake::new(width, height);
            (scene.setup)(&mut fake);
            let base = write(&out, fonts.as_ref(), scene.name, &fake.app)?;
            let _ = writeln!(
                index,
                "- {width}x{height}: [png]({base}.png) · [txt]({base}.txt) · [ansi]({base}.ansi)"
            );
        }
        index.push('\n');
    }
    // Below the minimum size the TUI only says so.
    let small = Fake::new(36, 7);
    let base = write(&out, fonts.as_ref(), "small-terminal", &small.app)?;
    let _ = writeln!(
        index,
        "## small-terminal\n\nbelow the minimum size\n\n- 36x7: [png]({base}.png) · [txt]({base}.txt) · [ansi]({base}.ansi)\n"
    );
    fs::write(out.join("index.md"), index)?;
    println!("{}", out.join("index.md").display());
    Ok(())
}
