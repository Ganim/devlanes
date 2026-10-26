# Terminals

devlanes uses a few terminal features beyond plain text: a progress bar and a title in the tab, clickable links, desktop notifications, marks to jump between sections of long output, and the clipboard from the TUI. Each is optional and configured in `~/.devlanes/config.yaml`:

```yaml
terminal:
  progress: auto        # auto, always or never
  title: auto
  links: auto
  notifications: auto
  marks: auto
```

`devlanes config set terminal.title never` changes one setting without opening the file, and `devlanes config get terminal` prints the values in effect.

`auto` uses a feature only on human output to a terminal known to support it, never inside tmux or screen, in CI or under `TERM=dumb`. Agent, JSON, plain and piped output never get any of them. `DEVLANES_TERM_PROGRESS`, `DEVLANES_TERM_TITLE`, `DEVLANES_TERM_LINKS`, `DEVLANES_TERM_NOTIFICATIONS` and `DEVLANES_TERM_MARKS` set to `0` or `1` override the settings for one command.

To work in a unit from another terminal, run `devlanes attach <lane>:<unit>` there, or `devlanes env shell` for a shell with the lane's environment. Older configs that name a terminal app (`terminal: ghostty`, `terminal.app`) still load; the name is ignored and `devlanes doctor` warns about it.

## Progress in the tab

Starts (`start`, `restart`, `lane resume`, `lane new --start`, `wait`) draw their progress in the terminal's tab or title bar with OSC 9;4, the sequence Windows Terminal introduced and iTerm2, Ghostty, WezTerm, Konsole and Ptyxis adopted. The bar turns red for a second on failure and is cleared on success, Ctrl-C or exit.

`auto` draws it when stdout is a terminal that is known to support it: iTerm2 3.6.6+, WezTerm, Ghostty, Windows Terminal, ConEmu, Konsole 26.04+, Ptyxis 48+. It stays off in CI, under `TERM=dumb`, and inside tmux or screen, because tmux swallows the sequence unless `set -g allow-passthrough on` is in `.tmux.conf`; with that line, `always` works through tmux. kitty and Terminal.app do not draw it. `DEVLANES_TERM_PROGRESS=0` or `1` overrides the setting for one command.

The TUI draws the same bar while a start runs inside it.

## Tab title

The TUI titles the tab with the lane on screen and how it stands: `devlanes · acme@0`, `devlanes · acme@0 · starting 16/44`, `devlanes · acme@0 · 1 failed`. Long commands do the same: `devlanes start acme@0 · 3/8` for `start`, `restart`, `lane resume`, `lane new --start`, `wait` and `with`, and `devlanes logs -f api` or `devlanes attach api`. Short commands leave the title alone.

devlanes saves the previous title on the terminal's title stack and puts it back when it exits, also after Ctrl-C or a crash. `auto` therefore sets a title only in terminals that keep that stack: xterm, iTerm2, kitty, Ghostty and VTE terminals such as GNOME Terminal and Ptyxis. WezTerm, Konsole, Windows Terminal and Terminal.app would keep devlanes' title after it exits, so they get none unless you choose `always`. Inside tmux, tmux owns the titles; `always` sets the pane title there.

## Links

Service URLs in `start`, `status` and `url ls`, config error locations (`devlanes.yaml:5:12`) and the path `debug bundle` writes are clickable (OSC 8) in iTerm2, kitty, WezTerm, Ghostty, Windows Terminal, VTE 0.50+, Konsole, Alacritty and foot. iTerm2 opens a config error at its line and column. The TUI links the URLs shown in the pane. Elsewhere the same text prints without a link.

## Notifications

The TUI turns on focus reports and, while its terminal is not focused, sends a desktop notification when a start that took 10 seconds or more ends, and when a running service crashes. At most one notification goes out every 10 seconds. kitty gets OSC 99, Ghostty, WezTerm and VTE terminals OSC 777, iTerm2 and foot OSC 9, and other terminals the bell. GNOME Terminal shows OSC 777 only with the patch Fedora and Ubuntu ship.

A command cannot see whether its terminal is focused, so `start` and the other long commands notify at the end of a long start only in kitty, which itself skips a focused window. Set `notifications: always` to have them notify in any terminal.

## Section marks

`logs` for several services and a failed start's summary mark where each section begins, so the terminal's jump keys move between them: iTerm2 gets its own marks (cmd-shift-up and down), WezTerm, Ghostty, kitty, foot and Windows Terminal get OSC 133 prompt marks (their jump-to-prompt keys). With shell integration on, a terminal may then count the sections as separate commands.

## Clipboard

In the TUI, `y` (and `c`, `v`) copies the selection with OSC 52, which writes to the clipboard through the terminal, so it works over ssh. Terminals that block OSC 52 (some by default, kitty asks on first use) drop the write silently; the TUI says `copied (OSC 52; some terminals block it)` because it cannot tell. Selections over 100 KiB are refused, since terminals truncate them.

## Colours and glyphs

The TUI wraps each frame in synchronized output (`CSI ?2026h`/`l`) so terminals that support it never show a half-drawn screen, and redraws a few times a second instead of sixty while its terminal is not focused. Terminals without either feature ignore them.

`tui.theme: auto` uses true colour only when the terminal advertises it (`COLORTERM=truecolor`) and reports its background (`COLORFGBG`), otherwise the 16 ANSI colours; `dark`, `light` or `ansi` force one. `NO_COLOR` turns colours off everywhere, `CLICOLOR_FORCE` keeps them in pipes. `tui.glyphs: ascii` replaces the Unicode state symbols for fonts or locales without them; `auto` picks by `LANG`.

A unit's colour is the first of its tags with a `tags.<name>.color` in `devlanes.yaml`, else the first category word in its tags, preset or name (`api` blue, `web` green, `worker` magenta, …), else a hash of its first tag or name; red is reserved for failures.

## Windows consoles

On Windows the CLI turns on escape-sequence processing for its console when it starts. A console that refuses it gets plain output with colours and every feature above turned off, whatever the settings say.

Windows Terminal gets the progress bar, links, OSC 133 marks and bell notifications under `auto`, and no title unless you choose `always`, as listed above. A classic console window (conhost) is not detected as any of the terminals above, so under `auto` it gets bell notifications and none of the other features; `always` still writes them. Copying from the TUI with OSC 52 works in Windows Terminal; the classic console window does not support OSC 52. If its font lacks the state symbols, set `tui.glyphs: ascii`.

Git Bash's own window (mintty) gives Windows programs no console, so prompts are cancelled with a message and `attach` fails there; use Windows Terminal or a console window for those. [Windows](windows.md#commands-you-type) has more on shells.
