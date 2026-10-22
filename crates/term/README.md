# devlanes-term

Terminal integrations for devlanes clients: the `TerminalLauncher` trait behind "open in terminal" in the TUI, and the terminal tab's progress bar during starts. Each terminal application has its own `devlanes-term-*` launcher crate, and `bins/devlanes` registers them. `devlanes-cli` and `devlanes-tui` use the progress bar.

## Usage

```rust
use devlanes_term::{LaunchRequest, TerminalLauncher};

fn open(launcher: &dyn TerminalLauncher) -> Result<(), devlanes_term::LaunchError> {
    launcher.launch(&LaunchRequest {
        cwd: "/work/acme".into(),
        command: vec!["devlanes".into(), "attach".into(), "acme@2:api".into()],
        title: Some("acme@2:api".into()),
    })
}
```

`launch` blocks while a short helper such as `osascript` or `open` runs, so call it from `spawn_blocking` in async code.

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

- `TerminalLauncher::name` is the user config value that selects the launcher (`terminal: kitty`).
- `TerminalLauncher::invocation` plans the helper process as data; `launch` runs it.
- `Invocation::run` starts the helper with null stdio and reaps long-lived terminal processes on a background thread.
- `shell_quote`, `shell_join` and `applescript_string` quote commands for terminals that take shell text.
- `Progress::sequence` encodes `OSC 9;4` (`ESC ] 9 ; 4 ; state ; percent ESC \`).
- `progress_support` decides whether to draw the bar from the setting, the TTY and the environment.
- `ProgressBar` writes it and clears it on drop, including while a panic unwinds.
- `StartTracker` turns lane events into progress: ready services over the units the start's `services:started` event plans, newest `start_id` per lane. `command()` reads the events a start request streams; `viewer()` reads `subscribe` and `status` for the TUI.
- `phase_detail` words a phase's `progress` event (`waiting for postgres (1 of 2 healthy)`, `migrate (2 of 3)`); the CLI's phase lines and the TUI header both use it.

## Progress bar

Auto mode follows cargo's `term.progress` detection: Windows Terminal, ConEmu, WezTerm, Ghostty, Ptyxis 48+, Konsole 26.04+, and iTerm2 3.6.6+ (older iTerm2 posts `OSC 9` as a notification). kitty exports no version and older releases showed the sequence as a notification, so it needs `always`. Auto is off inside tmux, screen, CI and dumb terminals; `always` inside tmux wraps the sequence for passthrough, which tmux forwards only with `allow-passthrough on`.

`DEVLANES_TERM_PROGRESS=0` or `1` overrides the setting. Piped output never gets the bar. Ghostty drops a bar that is not repeated within about 15 seconds, so callers resend it every second.

## Config example

```yaml
# ~/.devlanes/config.yaml: terminal app the TUI's O key opens a unit in,
# and the tab progress bar during starts (auto, always or never).
terminal:
  app: wezterm
  progress: auto
```

`terminal: wezterm` alone works too and keeps progress on auto. The app is one of `ghostty`, `iterm2`, `kitty`, `wezterm` or `terminal`, and picks the launcher whose `name` matches. Without it the TUI uses Terminal.app on macOS, else the first launcher that is installed.

## License

Apache-2.0
