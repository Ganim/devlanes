# Terminals

devlanes integrates with the terminal in three places: opening a unit in a new terminal window, a progress bar in the tab during starts, and the clipboard from the TUI. All of it is optional and configured in `~/.devlanes/config.yaml`:

```yaml
terminal:
  app: ghostty          # iterm2, ghostty, wezterm, kitty or terminal
  progress: auto        # auto, always or never
```

`terminal: ghostty` alone is accepted too.

## Open in a new terminal

In the TUI, `O` on a unit opens a new window of your terminal app running `devlanes attach <lane>:<unit>` in the lane's worktree; `O` on a lane opens your shell there. There is no CLI command for it; `devlanes attach` and `devlanes env shell` are the equivalents from a shell you already have.

| App | macOS | Linux |
| --- | --- | --- |
| iTerm2 | AppleScript: new window, title set, command written | not available |
| Terminal.app | AppleScript `do script`, title set | not available |
| Ghostty | `open -na Ghostty.app --args --working-directory … -e …` | `ghostty --working-directory … -e …` |
| kitty | `open -na kitty.app --args --directory … --title …` | `kitty --single-instance --directory …` |
| WezTerm | `open -na WezTerm.app --args start --cwd … -- …` | `wezterm start --cwd … -- …` |

No remote-control protocol is used, so kitty needs no `allow_remote_control` and WezTerm no mux server. iTerm2 and Terminal.app go through `osascript`, which asks macOS once for Automation permission for the terminal that runs the TUI; a denied permission shows up as `osascript exited with status 1`.

When `terminal.app` is unset, the TUI uses the terminal it is running in (from `TERM_PROGRAM`, `KITTY_WINDOW_ID`, `WEZTERM_PANE` and similar), then Terminal.app on macOS, then the first installed app in the order iTerm2, Ghostty, WezTerm, kitty. Those variables survive ssh and tmux, so a TUI on a Linux box opened from iTerm2 tries iTerm2 and fails; set `terminal.app` on the box. GNOME Terminal, Konsole, Alacritty and foot have no launcher yet.

## Progress in the tab

Starts (`start`, `restart`, `lane resume`, `lane new --start`, `wait`) draw their progress in the terminal's tab or title bar with OSC 9;4, the sequence Windows Terminal introduced and iTerm2, Ghostty, WezTerm, Konsole and Ptyxis adopted. The bar turns red for a second on failure and is cleared on success, Ctrl-C or exit.

`auto` draws it when stdout is a terminal that is known to support it: iTerm2 3.6.6+, WezTerm, Ghostty, Windows Terminal, ConEmu, Konsole 26.04+, Ptyxis 48+. It stays off in CI, under `TERM=dumb`, and inside tmux or screen, because tmux swallows the sequence unless `set -g allow-passthrough on` is in `.tmux.conf`; with that line, `always` works through tmux. kitty and Terminal.app do not draw it. `DEVLANES_TERM_PROGRESS=0` or `1` overrides the setting for one command.

The TUI draws the same bar while a start runs inside it.

## Clipboard

In the TUI, `y` (and `c`, `v`) copies the selection with OSC 52, which writes to the clipboard through the terminal, so it works over ssh. Terminals that block OSC 52 (some by default, kitty asks on first use) drop the write silently; the TUI says `copied (OSC 52; some terminals block it)` because it cannot tell. Selections over 100 KiB are refused, since terminals truncate them.

## Colours and glyphs

`tui.theme: auto` uses true colour only when the terminal advertises it (`COLORTERM=truecolor`) and reports its background (`COLORFGBG`), otherwise the 16 ANSI colours; `dark`, `light` or `ansi` force one. `NO_COLOR` turns colours off everywhere, `CLICOLOR_FORCE` keeps them in pipes. `tui.glyphs: ascii` replaces the Unicode state symbols for fonts or locales without them; `auto` picks by `LANG`.

Unit colours come from tags (`api` blue, `web` green, `worker` magenta, …) or `tags.<name>.color` in `devlanes.yaml`; red is reserved for failures.
