# devlanes-term

Terminal integrations for devlanes clients: the tab's progress bar and title, clickable links, desktop notifications and section marks, each with the detection of the terminals that support it. `devlanes-cli`, `devlanes-tui` and the `devlanes` binary use it; it writes nothing itself unless a caller hands it a writer.

## Usage

```rust
use devlanes_term::{Features, ProgressBar, Settings, StartTracker, TitleGuard};

fn run(tty: bool, tracker: &StartTracker) -> std::io::Result<()> {
    // Callers pass `tty: false` for agent, JSON, plain, CI or piped output.
    let features = Features::detect(&Settings::default(), tty, |key| std::env::var(key).ok());
    let mut title = TitleGuard::new(std::io::stdout(), features.title);
    title.set("devlanes start acme@0 · 3/8")?;
    let mut bar = ProgressBar::new(std::io::stdout(), features.progress);
    bar.set(tracker.progress())
    // Dropping `bar` clears it; dropping `title` puts the previous title back.
}
```

## API

- `Features::detect` decides every feature at once from the user's `terminal` settings, the TTY and the environment; `gate` is the check each one shares.
- `TitleGuard` pushes the current title (`CSI 22;0t`), sets titles with `OSC 0` only when the text changes, and pops it back (`CSI 23;0t`) on drop, also while a panic unwinds.
- `Links::url` and `Links::file` wrap text in `OSC 8`; file links carry `#line:column` only in iTerm2, which opens them there.
- `Notifications::sequence` encodes a notification as `OSC 99` (kitty), `OSC 777` (Ghostty, WezTerm, VTE), `OSC 9` (iTerm2, foot) or BEL; `Notifier` allows one per 10 seconds.
- `Marks::section` marks a heading: `OSC 1337;SetMark` in iTerm2, `OSC 133;A` and `C` where the terminal jumps between prompts.
- `ProgressBar` and `StartTracker` draw a start's progress (`OSC 9;4`); `StartTracker::counts` gives the same start in numbers for titles.

## Terminal support

Read from each terminal's documentation or source:

| Terminal | Title stack | Links | Notifications | Marks |
| --- | --- | --- | --- | --- |
| xterm | yes | no | BEL | no |
| iTerm2 | yes | yes, with line | OSC 9 | SetMark |
| kitty | yes | yes | OSC 99, unfocused only | OSC 133 |
| Ghostty | yes | yes | OSC 777 | OSC 133 |
| WezTerm | no (parsed, ignored) | yes | OSC 777 | OSC 133 |
| VTE (GNOME Terminal, Ptyxis) | yes | 0.50+ | OSC 777 (distribution patch) | no |
| Konsole | no (ignored) | yes | BEL | no |
| Windows Terminal | no | yes | BEL | OSC 133 |
| Alacritty | not detected | yes | BEL | no |
| foot | not detected | yes | OSC 9 | OSC 133 |
| Apple Terminal | not documented | no | BEL | no |

Auto never writes inside tmux or screen, in CI or to a dumb terminal; `always` writes everywhere, and inside tmux notifications go through passthrough (`allow-passthrough on`). A command cannot see whether its terminal is focused, so with auto it notifies only in kitty, which skips focused windows itself; the TUI turns on focus reports (`CSI ?1004h`) and notifies only while unfocused.

## Progress bar

Auto mode follows cargo's `term.progress` detection: Windows Terminal, ConEmu, WezTerm, Ghostty, Ptyxis 48+, Konsole 26.04+, and iTerm2 3.6.6+ (older iTerm2 posts `OSC 9` as a notification). kitty exports no version and older releases showed the sequence as a notification, so it needs `always`. `always` inside tmux wraps the sequence for passthrough. Ghostty drops a bar that is not repeated within about 15 seconds, so callers resend it every second.

## Config example

```yaml
# ~/.devlanes/config.yaml; each is auto, always or never.
terminal:
  progress: auto
  title: auto
  links: auto
  notifications: auto
  marks: auto
```

`DEVLANES_TERM_PROGRESS`, `DEVLANES_TERM_TITLE`, `DEVLANES_TERM_LINKS`, `DEVLANES_TERM_NOTIFICATIONS` and `DEVLANES_TERM_MARKS` set to `0` or `1` override the settings.

## License

Apache-2.0
