# devlanes-term

Terminal integrations for devlanes clients: the terminal tab's progress bar during starts, the detection of terminals that draw it, and opening URLs in the browser. `devlanes-cli` and `devlanes-tui` use it.

## Usage

```rust
use devlanes_term::{ProgressBar, ProgressMode, StartTracker, progress_support_from_env};

fn draw(tty: bool, tracker: &StartTracker) -> std::io::Result<()> {
    let wrap = progress_support_from_env(ProgressMode::Auto, tty);
    let mut bar = ProgressBar::new(std::io::stdout(), wrap);
    bar.set(tracker.progress())
    // Dropping `bar` clears it.
}
```

## API

- `Progress::sequence` encodes `OSC 9;4` (`ESC ] 9 ; 4 ; state ; percent ESC \`).
- `progress_support` decides whether to draw the bar from the setting, the TTY and the environment.
- `ProgressBar` writes it and clears it on drop, including while a panic unwinds.
- `StartTracker` turns lane events into progress: ready services over the units the start's `services:started` event plans, newest `start_id` per lane. `command()` reads the events a start request streams; `viewer()` reads `subscribe` and `status` for the TUI.
- `phase_detail` words a phase's `progress` event (`waiting for postgres (1 of 2 healthy)`, `migrate (2 of 3)`); the CLI's phase lines and the TUI header both use it.
- `open_url` opens a URL in the default browser for `devlanes url open` and the TUI: `open` on macOS, the shell on Windows, `xdg-open` on Linux, and inside WSL the Windows browser through `wslview`, then Windows' `rundll32.exe url.dll,FileProtocolHandler`. No opener goes through a shell, so `&` in a URL is safe. `Output::Terminal` lets the opener use the terminal (the CLI), `Output::Hidden` keeps it off the screen and puts its last stderr line into the error (the TUI). Blocking.

## Progress bar

Auto mode follows cargo's `term.progress` detection: Windows Terminal, ConEmu, WezTerm, Ghostty, Ptyxis 48+, Konsole 26.04+, and iTerm2 3.6.6+ (older iTerm2 posts `OSC 9` as a notification). kitty exports no version and older releases showed the sequence as a notification, so it needs `always`. Auto is off inside tmux, screen, CI and dumb terminals; `always` inside tmux wraps the sequence for passthrough, which tmux forwards only with `allow-passthrough on`.

`DEVLANES_TERM_PROGRESS=0` or `1` overrides the setting. Piped output never gets the bar. Ghostty drops a bar that is not repeated within about 15 seconds, so callers resend it every second.

## Config example

```yaml
# ~/.devlanes/config.yaml: the tab progress bar during starts (auto, always or never).
terminal:
  progress: auto
```

## License

Apache-2.0
