//! Dialogs: confirmations, the profile picker and the key list. They keep a
//! box because they float over content.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};

use super::fit;
use crate::app::{App, Dialog, ProfileChoice, ProfilesState, View};
use crate::text::{self, clean, clip};
use crate::theme::{Glyph, Theme, Tone};

/// Every binding, grouped by where it works. `None` keys are section names.
const KEYS: &[(&str, &[(&str, &str)])] = &[
    (
        "list",
        &[
            ("j k  up down", "move; k from the top selects the lane"),
            ("g G", "the lane, the last row"),
            ("h l", "fold, unfold; h moves to the section"),
            (
                "enter  space",
                "fold a section, or type into a running unit",
            ),
            (
                "[ ]  1-9",
                "previous, next lane; lane by its place in the bar",
            ),
            (
                "n N  e",
                "next, previous problem; every error in full (or click it)",
            ),
            ("t  i", "jump to the tasks, the infra"),
            (
                "s  x  r",
                "start, stop, restart the selection; s runs a task",
            ),
            ("s  S", "on the lane: its default services; every service"),
            ("p", "start the lane with chosen profiles"),
            ("C", "clean the lane (asks first)"),
            ("o", "open the unit's address in a browser"),
            ("/", "filter by name or +tag; enter keeps, esc clears"),
        ],
    ),
    (
        "pane",
        &[
            ("enter, click", "type into the unit"),
            (
                "esc  ctrl+]",
                "stop typing (ctrl+] in full-screen programs)",
            ),
            ("z", "zoom the pane"),
            (
                "pgup pgdn  wheel",
                "scroll; older lines come from stored logs",
            ),
            ("c  v  y", "select text, mark, copy (OSC 52)"),
            ("drag", "select text and copy it"),
        ],
    ),
    (
        "monitor (M)",
        &[
            ("< >", "sort by the previous, next column"),
            ("i", "reverse the sort"),
            ("/  e", "filter, grouped or flat"),
            ("h l", "fold, unfold a lane"),
            ("z", "detail graphs fill the page"),
            (
                "c  m",
                "cpu or memory in the big box (memory: a square per core)",
            ),
            ("wheel", "scroll the table; the selection stays"),
            ("s  x  r", "start, stop, restart the unit or lane"),
            ("enter", "open the unit in its project"),
        ],
    ),
    (
        "daemon (D)",
        &[
            ("R  S", "restart, stop or start the daemon"),
            ("X", "clean every lane whose worktree is gone"),
            ("tab", "move j k g G and c between the checks and the log"),
            (
                "j k  g G",
                "scroll the log (pauses), oldest, newest and follow",
            ),
            ("w  /  T", "level floor, text filter, targets"),
            ("F  d", "pause or follow, fold or unfold repeats"),
            (
                "c",
                "copy mode: v select, y copy, Y whole entries as in the file",
            ),
            ("drag", "select text in either box and copy it"),
            ("i", "every check, or problems only"),
        ],
    ),
    (
        "everywhere",
        &[
            (
                "tab  shift+tab",
                "next, previous project (not on the daemon page)",
            ),
            ("shift+1-9", "project by number"),
            ("M  D", "monitor, daemon; again or esc returns"),
            ("esc", "one step back, then the menu: options, help, quit"),
            ("u", "hide the version notice"),
            ("q", "quit; asks first while lanes run (tui.on_quit)"),
        ],
    ),
];

/// The section of the screen in view, listed first in the help.
fn first_section(app: &App) -> &'static str {
    match app.view {
        View::Monitor => "monitor (M)",
        View::Daemon => "daemon (D)",
        View::Project => "list",
    }
}

/// Fits a dialog into `rows`: the tail always shows; the body loses its last
/// lines, the final kept one replaced by `…`.
fn compact(
    mut body: Vec<Line<'static>>,
    tail: Vec<Line<'static>>,
    rows: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // A blank line separates body and tail when there is room for it.
    if body.len() + tail.len() < rows && !body.is_empty() {
        body.push(Line::from(""));
    }
    if body.len() + tail.len() <= rows {
        body.extend(tail);
        return body;
    }
    if tail.len() >= rows {
        let skip = tail.len() - rows;
        return tail.into_iter().skip(skip).collect();
    }
    let room = rows - tail.len();
    body.truncate(room.saturating_sub(1));
    body.push(Line::styled("…", theme.quiet()));
    body.extend(tail);
    body
}

/// First index of a `room`-row window over `len` items that shows `selected`.
fn window(len: usize, selected: usize, room: usize) -> usize {
    if len <= room {
        return 0;
    }
    selected.saturating_sub(room / 2).min(len - room)
}

/// `key action   key action`: the keys bold, the labels dim.
fn keys(theme: &Theme, pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, label)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled((*key).to_owned(), theme.key()));
        spans.push(Span::styled(format!(" {label}"), theme.quiet()));
    }
    Line::from(spans)
}

/// `▌[x] web      default    +web, +api`: the checkbox and name, then the
/// markers, then the include selectors dimmed.
fn profile_row(theme: &Theme, item: &ProfileChoice, selected: bool, width: usize) -> Line<'static> {
    let check = if item.checked { "[x]" } else { "[ ]" };
    let profile = &item.profile;
    let marker = if selected {
        theme.glyph(Glyph::Marker)
    } else {
        " "
    };
    let mut spans = vec![
        Span::styled(marker, theme.focus()),
        Span::raw(format!("{check} ")),
        Span::styled(
            format!("{:<9}", clean(&profile.name)),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<9}", if profile.is_default { "default" } else { "" }),
            theme.quiet(),
        ),
        Span::styled(
            format!("{:<9}", if profile.skips_infra { "no infra" } else { "" }),
            theme.quiet(),
        ),
    ];
    if !profile.include.is_empty() {
        spans.push(Span::styled(
            clean(&profile.include.join(", ")),
            theme.quiet(),
        ));
    }
    let line = fit(spans, width);
    if selected {
        line.patch_style(theme.selection())
    } else {
        line
    }
}

/// Modal dialogs over a cleared box.
pub fn render(app: &App, dialog: &Dialog, frame: &mut Frame, area: Rect) {
    let theme = &app.theme;
    // Text wraps to the widest box that fits, leaving a column of margin each side.
    let max = usize::from(area.width.saturating_sub(6)).max(1);
    let narrow = max.min(64);
    let wrapped = |text: &str, width: usize, style: Style| -> Vec<Line<'static>> {
        text::wrap(&clean(text), width)
            .into_iter()
            .map(|line| Line::styled(line, style))
            .collect()
    };
    let rows = usize::from(area.height.saturating_sub(2)).max(1);
    // `body` may be cut to fit; `tail` (the keys) always stays.
    let (title, body, tail): (String, Vec<Line>, Vec<Line>) = match dialog {
        // The Esc menu and its options page draw themselves.
        Dialog::Menu(menu) => return crate::menu::render(theme, menu, app.version, frame, area),
        Dialog::Settings(settings) => {
            return crate::settings::render(theme, settings, frame, area);
        }
        Dialog::Welcome(welcome) => return crate::welcome::render(theme, welcome, frame, area),
        Dialog::Confirm {
            title,
            prompt,
            detail,
            action,
            strict,
            ..
        } => {
            let mut body = wrapped(prompt, narrow, Style::new().add_modifier(Modifier::BOLD));
            if !detail.is_empty() {
                body.push(Line::from(""));
            }
            for sentence in detail {
                body.extend(wrapped(sentence, narrow, Style::new()));
            }
            let tail = if *strict {
                keys(theme, &[("y", action), ("n", "cancel")])
            } else {
                keys(theme, &[("enter", action), ("esc", "cancel")])
            };
            (title.clone(), body, vec![tail])
        }
        Dialog::Targets(picker) => {
            let width = max.min(56);
            let tail = vec![
                Line::styled("With none checked, every target shows.", theme.quiet()),
                Line::from(""),
                keys(
                    theme,
                    &[
                        ("space", "check"),
                        ("a", "none"),
                        ("enter", "apply"),
                        ("esc", "cancel"),
                    ],
                ),
            ];
            let room = rows.saturating_sub(tail.len() + 1).max(1);
            let start = window(picker.items.len(), picker.index, room);
            let body = picker
                .items
                .iter()
                .enumerate()
                .skip(start)
                .take(room)
                .map(|(index, (target, count, checked))| {
                    let selected = index == picker.index;
                    let marker = if selected {
                        theme.glyph(Glyph::Marker)
                    } else {
                        " "
                    };
                    let check = if *checked { "[x]" } else { "[ ]" };
                    let name_width = width.saturating_sub(12);
                    let line = fit(
                        vec![
                            Span::styled(marker, theme.focus()),
                            Span::raw(format!("{check} ")),
                            Span::raw(format!(
                                "{:<name_width$} ",
                                clip(&clean(target), name_width)
                            )),
                            Span::styled(format!("{count:>5}"), theme.quiet()),
                        ],
                        width,
                    );
                    if selected {
                        line.patch_style(theme.selection())
                    } else {
                        line
                    }
                })
                .collect();
            ("daemon.log targets".to_owned(), body, tail)
        }
        Dialog::Quit(quit) => {
            let lanes = quit.lanes.join(", ");
            let prompt = if quit.lanes.len() == 1 {
                format!("{lanes} is running.")
            } else {
                format!("{lanes} are running.")
            };
            let mut body = wrapped(&prompt, narrow, Style::new().add_modifier(Modifier::BOLD));
            body.push(Line::from(""));
            for (index, choice) in crate::app::QUIT_CHOICES.iter().enumerate() {
                let selected = index == quit.index;
                let marker = if selected {
                    theme.glyph(Glyph::Marker)
                } else {
                    " "
                };
                let line = fit(
                    vec![
                        Span::styled(marker, theme.focus()),
                        Span::raw(format!("{choice} ")),
                    ],
                    narrow,
                );
                body.push(if selected {
                    line.patch_style(theme.selection())
                } else {
                    line
                });
            }
            body.push(Line::from(""));
            let check = if quit.remember { "[x]" } else { "[ ]" };
            body.push(Line::from(vec![
                Span::raw(format!(" {check} remember this choice ")),
                Span::styled("(tui.on_quit)", theme.quiet()),
            ]));
            let tail = keys(
                theme,
                &[
                    ("enter", "choose"),
                    ("space", "remember"),
                    ("esc", "cancel"),
                ],
            );
            ("quit".to_owned(), body, vec![tail])
        }
        Dialog::Profiles { lane, state } => {
            let mut body = Vec::new();
            let tail = match state {
                ProfilesState::Loading => {
                    body.extend(wrapped("loading…", narrow, theme.quiet()));
                    vec![keys(theme, &[("esc", "cancel")])]
                }
                ProfilesState::Unsupported => {
                    body.extend(wrapped(
                        "This daemon is too old for profiles. Upgrade devlanes and restart the daemon.",
                        narrow,
                        Style::new(),
                    ));
                    vec![keys(theme, &[("esc", "close")])]
                }
                ProfilesState::Ready { items, .. } if items.is_empty() => {
                    body.extend(wrapped(
                        "devlanes.yaml defines no profiles.",
                        narrow,
                        Style::new(),
                    ));
                    vec![keys(theme, &[("esc", "close")])]
                }
                ProfilesState::Ready { items, index } => {
                    // Matches what Enter sends with nothing checked.
                    let empty = if items.iter().any(|item| item.profile.is_default) {
                        "With none checked, enter starts the default profile."
                    } else {
                        "With none checked, enter starts the lane without profiles."
                    };
                    let mut tail = wrapped(empty, narrow, theme.quiet());
                    tail.push(Line::from(""));
                    tail.push(keys(
                        theme,
                        &[("space", "check"), ("enter", "start"), ("esc", "cancel")],
                    ));
                    let width = max.min(72);
                    let room = rows.saturating_sub(tail.len() + 1).max(1);
                    let start = window(items.len(), *index, room);
                    body.extend(
                        items
                            .iter()
                            .enumerate()
                            .skip(start)
                            .take(room)
                            .map(|(i, item)| profile_row(theme, item, i == *index, width)),
                    );
                    tail
                }
            };
            (format!("start {} with profiles", clean(lane)), body, tail)
        }
        Dialog::Errors(state) => {
            let Some(entry) = app.errors.get(state.index) else {
                return;
            };
            let width = max.min(100);
            let mut text = Vec::new();
            for (index, line) in entry.lines().iter().enumerate() {
                let style = if index == 0 {
                    theme.tone(Tone::Failure).add_modifier(Modifier::BOLD)
                } else if line.starts_with("command: ") || line.starts_with("run: ") {
                    theme.quiet()
                } else {
                    Style::new()
                };
                text.extend(wrapped(line, width, style));
            }
            // The keys take one row and the blank line before them another.
            let room = rows.saturating_sub(2).max(1);
            let max_scroll = text.len().saturating_sub(room);
            state.max_scroll.set(max_scroll);
            let scroll = state.scroll.min(max_scroll);
            let mut pairs = Vec::new();
            if app.errors.len() > 1 {
                pairs.push(("h l", "older, newer"));
            }
            if max_scroll > 0 {
                pairs.push(("j k", "scroll"));
            }
            pairs.extend([("y", "copy"), ("esc", "close")]);
            let tail = vec![keys(theme, &pairs)];
            let mut title = if app.errors.len() > 1 {
                format!("error {} of {}", state.index + 1, app.errors.len())
            } else {
                "error".to_owned()
            };
            // Says there is more below, and how far down the view is.
            if max_scroll > 0 {
                let shown = (scroll + room).min(text.len());
                title.push_str(&format!(
                    " · lines {}-{shown} of {}",
                    scroll + 1,
                    text.len()
                ));
            }
            let body = text.into_iter().skip(scroll).take(room).collect();
            (title, body, tail)
        }
        Dialog::Help => {
            let first = first_section(app);
            let mut sections: Vec<&(&str, &[(&str, &str)])> = KEYS.iter().collect();
            sections.sort_by_key(|(name, _)| *name != first);
            // The glyphs first: normal states have no word in the list, so
            // this is where their meaning is written down.
            let legend = [
                (
                    theme.glyph(Glyph::Ready),
                    theme.tone(Tone::Ok),
                    "ready (check passed)",
                ),
                (
                    theme.glyph(Glyph::Running),
                    theme.tone(Tone::Ok),
                    "running (no ready check)",
                ),
                (
                    theme.glyph(Glyph::Stopped),
                    theme.quiet(),
                    "stopped, not run",
                ),
                (theme.spinner(3), Style::new(), "starting, stopping"),
                (theme.glyph(Glyph::Queued), theme.quiet(), "queued"),
                (
                    theme.glyph(Glyph::Failed),
                    theme.tone(Tone::Failure),
                    "failed",
                ),
                (
                    theme.glyph(Glyph::Orphaned),
                    theme.tone(Tone::Attention),
                    "orphaned",
                ),
                (theme.glyph(Glyph::Done), theme.tone(Tone::Ok), "task done"),
                (theme.glyph(Glyph::Bar), Style::new(), "the unit's color"),
            ];
            let mut body = vec![Line::styled("glyphs", theme.quiet())];
            for pair in legend.chunks(3) {
                let mut line = vec![Span::raw("  ")];
                for (glyph, style, word) in pair {
                    line.push(Span::styled(format!("{glyph} "), *style));
                    line.push(Span::raw(format!("{word:<26}")));
                }
                body.push(fit(line, max));
            }
            for (name, pairs) in sections {
                if !body.is_empty() {
                    body.push(Line::from(""));
                }
                body.push(Line::styled((*name).to_owned(), theme.quiet()));
                for (key, what) in *pairs {
                    body.push(fit(
                        vec![
                            Span::styled(format!("  {key:<18}"), theme.key()),
                            Span::raw((*what).to_owned()),
                        ],
                        max,
                    ));
                }
            }
            (
                "keys".into(),
                body,
                vec![Line::styled("any key closes", theme.quiet())],
            )
        }
    };
    let lines = compact(body, tail, rows, theme);
    // Wide enough for the title too, which is cut only on a narrow screen.
    let content_width = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .max(text::width(&clean(&title)) + 2);
    let width = u16::try_from(content_width + 4)
        .unwrap_or(area.width)
        .clamp(20.min(area.width), area.width);
    let height = u16::try_from(lines.len() + 2)
        .unwrap_or(area.height)
        .min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    *app.dialog_area.borrow_mut() = Some(rect);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(theme.focus())
        .title(Span::raw(format!(
            " {} ",
            clip(&clean(&title), usize::from(width.saturating_sub(4)))
        )));
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(block.padding(ratatui::widgets::Padding::horizontal(1))),
        rect,
    );
}
