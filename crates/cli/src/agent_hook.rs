//! The coding-agent `PreToolUse` hook that `agent-plugin/hooks/hooks.json` runs as
//! `devlanes __agent-hook`. It rejects shell commands that start a dev server in
//! the background inside a devlanes project and points to `devlanes exec`.
//!
//! Claude Code and Codex both pass the tool call as one JSON object on stdin.
//! A blocked call gets a `permissionDecision: "deny"` object on stdout and exit
//! 0, which both agents honour; every other outcome is exit 0 with no output,
//! which leaves the normal permission flow alone. A payload this hook cannot
//! read is allowed. Exit 2 is never used: the plugin's command masks the exit
//! status so that an older devlanes, whose usage error also exits 2, cannot
//! block every shell call.

use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

/// First argument that runs the hook instead of the CLI. Hidden: agents' hook
/// configs call it, people do not. Later arguments are ignored, because
/// `cmd.exe` passes the plugin command's `#` on as one.
pub const ARG: &str = "__agent-hook";

/// What the agent reads when a call is blocked.
pub const BLOCK_MESSAGE: &str = "\
Blocked by the devlanes plugin: this starts a long-running process in the background, where nothing supervises it and its logs are lost.
Use instead:
  devlanes start [UNIT...]               services defined in devlanes.yaml
  devlanes exec --name NAME -- COMMAND   any other long-running command
Then check it with `devlanes wait NAME --timeout 180s` and `devlanes logs NAME --tail 100`.
";

/// The hook's decision for one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Block,
}

/// Runs the hook as a process: reads the payload from stdin, prints
/// [`deny_output`] to stdout for a blocked call and returns the exit status,
/// always 0. `DEVLANES_BLOCK_BACKGROUND=0` in the agent's environment turns the
/// check off.
pub fn run() -> u8 {
    let mut input = Vec::new();
    // Drained before anything else, so the agent never writes into a closed pipe.
    if std::io::stdin().read_to_end(&mut input).is_err() {
        return 0;
    }
    if std::env::var_os("DEVLANES_BLOCK_BACKGROUND").is_some_and(|value| value == "0") {
        return 0;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = std::env::home_dir();
    if decide(&String::from_utf8_lossy(&input), &cwd, home.as_deref()) == Verdict::Block {
        let mut stdout = std::io::stdout().lock();
        // An agent that stopped reading gets no decision either way.
        let _ = writeln!(stdout, "{}", deny_output()).and_then(|()| stdout.flush());
    }
    0
}

/// The `PreToolUse` answer that denies the call with [`BLOCK_MESSAGE`] as the
/// reason the agent sees, in the shape Claude Code and Codex both read.
pub fn deny_output() -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": BLOCK_MESSAGE,
        }
    })
    .to_string()
}

/// Decides one `PreToolUse` payload. `cwd` stands in when the payload names no
/// directory, `home` resolves `cd ~`.
///
/// A command is blocked when all of these hold: it is detached (a lone `&`,
/// `setsid -f`, `tmux new -d`, `screen -dm`, `run_in_background`, and from
/// PowerShell or on Windows `Start-Process`, `Start-Job` and `start`); it looks
/// like a dev server and is not an editor; and the directory it runs in, after
/// any `cd`, is inside a project with `devlanes.yaml`.
pub fn decide(payload: &str, cwd: &Path, home: Option<&Path>) -> Verdict {
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(payload) else {
        return Verdict::Allow;
    };
    let input = &payload["tool_input"];
    let Some(command) = input["command"].as_str() else {
        return Verdict::Allow;
    };
    let background = input["run_in_background"].as_bool() == Some(true);
    let powershell_tool = payload["tool_name"].as_str() == Some("PowerShell");
    let windows = powershell_tool || BASH_MAY_RUN_WINDOWS_COMMANDS;
    let dialect = if powershell_tool || (windows && looks_like_powershell(command)) {
        Dialect::PowerShell
    } else {
        Dialect::Posix
    };
    let mut dir = match payload["cwd"].as_str() {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => cwd.to_path_buf(),
    };
    let check = Check {
        dialect,
        windows,
        home,
        depth: 0,
    };
    if check.judge(command, background, &mut dir) {
        Verdict::Block
    } else {
        Verdict::Allow
    }
}

/// Codex on Windows sends its PowerShell commands as the `Bash` tool, and Git
/// Bash ships a `start` that runs cmd's, so Windows `Bash` calls also get the
/// Windows launchers and path rules.
#[cfg(windows)]
const BASH_MAY_RUN_WINDOWS_COMMANDS: bool = true;

/// A Unix `Bash` call is POSIX shell only.
#[cfg(not(windows))]
const BASH_MAY_RUN_WINDOWS_COMMANDS: bool = false;

/// How deep `pwsh -c`, `iex`, `Start-Job { }` and `& { }` may nest before the
/// hook stops looking; real commands nest one or two levels.
const MAX_DEPTH: u8 = 6;

/// Whether a Windows `Bash` call is PowerShell text, which is how Codex sends
/// PowerShell: a cmdlet name such as `Set-Location` or `Start-Process`
/// (capitalised as PowerShell prints them, so `npm run start-server` does not
/// count), `$env:`, a statement that assigns `$name =` (a syntax error in
/// POSIX shells) or a here-string (`@'` ending a line).
fn looks_like_powershell(command: &str) -> bool {
    const VERBS: &[&str] = &[
        "Add", "Clear", "Copy", "Export", "ForEach", "Get", "Import", "Invoke", "Join", "Measure",
        "Move", "New", "Out", "Pop", "Push", "Receive", "Remove", "Rename", "Resolve", "Select",
        "Set", "Sort", "Split", "Start", "Stop", "Test", "Wait", "Where", "Write",
    ];
    let assigns = |statement: &str| {
        statement
            .trim_start()
            .strip_prefix('$')
            .and_then(|rest| rest.split_once('='))
            .is_some_and(|(name, _)| {
                let name = name.trim_end();
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
            })
    };
    command.contains("$env:")
        || command.split(['\n', ';']).any(assigns)
        || ["@'\n", "@\"\n", "@'\r\n", "@\"\r\n"]
            .iter()
            .any(|marker| command.contains(marker))
        || command
            .split(|c: char| c.is_whitespace() || matches!(c, ';' | '(' | '{' | '|' | '&'))
            .any(|word| {
                word.split_once('-').is_some_and(|(verb, noun)| {
                    VERBS.contains(&verb)
                        && noun.starts_with(|c: char| c.is_ascii_uppercase())
                        && noun.chars().all(|c| c.is_ascii_alphanumeric())
                })
            })
}

/// Quoting rules of the tool's shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// `sh`, bash and Git Bash: `\` escapes.
    Posix,
    /// PowerShell: `` ` `` escapes, `{ }` groups script blocks, `#` and
    /// `<# #>` comment, `@' '@` quotes lines, and a leading `&` calls a command.
    PowerShell,
}

/// Settings for judging the simple commands of one call.
#[derive(Debug, Clone, Copy)]
struct Check<'a> {
    dialect: Dialect,
    /// Windows commands and paths apply: PowerShell, cmd's `start`, `\`
    /// separators, `.exe`/`.cmd` names, case-insensitive commands, `$x =`
    /// assignments and the `&` call operator.
    windows: bool,
    home: Option<&'a Path>,
    /// How many shells or script blocks this text is nested in.
    depth: u8,
}

impl Check<'_> {
    /// Whether any simple command of `text` starts a dev server in the
    /// background inside a project. `detached` marks them all as detached, for
    /// `run_in_background` and script blocks that run as jobs.
    fn judge(&self, text: &str, detached: bool, dir: &mut PathBuf) -> bool {
        self.split(text)
            .into_iter()
            .any(|(tagged, segment)| self.blocks(&segment, tagged || detached, dir))
    }

    /// The same check for text that runs in PowerShell one level deeper:
    /// `pwsh -c TEXT`, `iex TEXT`, `Start-Job { TEXT }`, `& { TEXT }`.
    fn nested(&self) -> Option<Self> {
        (self.depth < MAX_DEPTH).then_some(Self {
            dialect: Dialect::PowerShell,
            windows: true,
            depth: self.depth + 1,
            ..*self
        })
    }

    /// Splits `command` into simple commands. Commands joined by `&&`, `||` or
    /// `|` share the flag of the operator that ends their list: `true` for a
    /// lone `&`. Operators inside quotes, PowerShell script blocks and
    /// redirections (`2>&1`, `&>`) don't split, and PowerShell comments are
    /// dropped.
    ///
    /// A Windows `Bash` call may be PowerShell or cmd text that reached the
    /// POSIX rules, where `"C:\acme\"` would escape its closing quote and
    /// swallow the rest of the line. So there, inside double quotes that start
    /// with a path (`C:`, `\\`, `.\`, `..\`), `\` is a plain character, as it is
    /// in PowerShell and cmd.
    fn split(&self, command: &str) -> Vec<(bool, String)> {
        let powershell = self.dialect == Dialect::PowerShell;
        let escape = if powershell { '`' } else { '\\' };
        let chars: Vec<char> = command.chars().collect();
        let mut segments = Vec::new();
        let mut list = Vec::new();
        let mut segment = String::new();
        let mut quote = None;
        // The text of the open double-quoted word, for the Windows path rule.
        let mut quoted = String::new();
        let mut braces = 0usize;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            i += 1;
            if quote == Some('\'') {
                segment.push(c);
                if c == '\'' {
                    quote = None;
                }
                continue;
            }
            let in_double = quote == Some('"');
            if c == escape && !(in_double && self.windows && windows_path(&quoted)) {
                segment.push(c);
                if in_double {
                    quoted.push(c);
                }
                if let Some(&escaped) = chars.get(i) {
                    segment.push(escaped);
                    if in_double {
                        quoted.push(escaped);
                    }
                    i += 1;
                }
                continue;
            }
            if in_double {
                segment.push(if c == '\n' { ' ' } else { c });
                if c == '"' {
                    quote = None;
                } else {
                    quoted.push(c);
                }
                continue;
            }
            if powershell {
                if c == '@'
                    && let Some(end) = here_string_end(&chars, i)
                {
                    segment.extend(
                        chars[i - 1..end]
                            .iter()
                            .map(|&c| if c == '\n' || c == '\r' { ' ' } else { c }),
                    );
                    i = end;
                    continue;
                }
                if c == '<' && chars.get(i) == Some(&'#') {
                    i = find(&chars, i + 1, &['#', '>']).map_or(chars.len(), |end| end + 2);
                    segment.push(' ');
                    continue;
                }
                if c == '#' && segment.chars().next_back().is_none_or(char::is_whitespace) {
                    i = find(&chars, i, &['\n']).unwrap_or(chars.len());
                    continue;
                }
                match c {
                    '{' => braces += 1,
                    '}' => braces = braces.saturating_sub(1),
                    _ => {}
                }
                // Line breaks stay: they separate the block's own statements.
                if braces > 0 || c == '}' {
                    if c == '\'' || c == '"' {
                        quote = Some(c);
                        quoted.clear();
                    }
                    segment.push(c);
                    continue;
                }
            }
            if c == '\'' || c == '"' {
                quote = Some(c);
                quoted.clear();
                segment.push(c);
                continue;
            }
            let next = chars.get(i).copied();
            let previous = i.checked_sub(2).map(|index| chars[index]);
            match c {
                '&' if next == Some('&') => {
                    add(&mut segment, &mut list);
                    i += 1;
                }
                '&' if next == Some('>') || matches!(previous, Some('>' | '<')) => segment.push(c),
                // PowerShell's call operator, as in `& 'C:\tools\vite.cmd' dev`;
                // in a POSIX shell a leading `&` is a syntax error.
                '&' if self.windows && segment.trim().is_empty() => segment.push(c),
                '&' => flush(true, &mut segment, &mut list, &mut segments),
                '|' => {
                    if matches!(next, Some('|' | '&')) {
                        i += 1;
                    }
                    add(&mut segment, &mut list);
                }
                ';' | '\n' => flush(false, &mut segment, &mut list, &mut segments),
                _ => segment.push(c),
            }
        }
        flush(false, &mut segment, &mut list, &mut segments);
        segments
    }

    /// Whether one simple command starts a dev server in the background inside
    /// a project. A `cd` moves `dir` for the commands after it.
    fn blocks(&self, segment: &str, detached: bool, dir: &mut PathBuf) -> bool {
        let mut words = self.words(segment);
        loop {
            match words.as_slice() {
                [first, ..] if is_assignment(first) => {
                    words.remove(0);
                }
                [first, ..] if matches!(*first, "nohup" | "exec" | "command" | "time" | "env") => {
                    words.remove(0);
                }
                [first, ..] if self.windows && *first == "&" => {
                    words.remove(0);
                }
                // `$job = Start-Job { ... }`
                [first, "=", ..] if self.windows && first.starts_with('$') => {
                    words.drain(..2);
                }
                // `$job=Start-Job { ... }`, `$job= Start-Job`
                [first, ..] if self.windows && first.starts_with('$') && first.contains('=') => {
                    match first.split_once('=').map(|(_, value)| value) {
                        Some(value) if !value.is_empty() => words[0] = value,
                        _ => {
                            words.remove(0);
                        }
                    }
                }
                _ => break,
            }
        }
        let Some(&first) = words.first() else {
            return false;
        };
        // `& { ... }` runs a script block here, so its `Set-Location` moves `dir`.
        if self.dialect == Dialect::PowerShell && first.starts_with('{') {
            return self.nested().is_some_and(|nested| {
                nested.judge(brace_body(tail(segment, first)), detached, dir)
            });
        }
        if self.is_cd(first) {
            self.change_dir(&words[1..], dir);
            return false;
        }
        let launch = match self.launch(segment, &words, detached, dir) {
            Some(Launched::Command(launch)) => launch,
            Some(Launched::Script {
                text,
                detached,
                mut dir,
            }) => {
                return self
                    .nested()
                    .is_some_and(|nested| nested.judge(&text, detached, &mut dir));
            }
            None => return false,
        };
        let Some(program) = launch.words.first() else {
            return false;
        };
        let program = self.program(program);
        // The backgrounded command is devlanes itself, such as devlanes logs -f.
        launch.detached
            && program != "devlanes"
            && !EDITORS.contains(&program.as_str())
            && self.long_running(&launch.words)
            && in_project(&launch.dir)
    }

    /// Splits a simple command into words on whitespace, like the unquoted
    /// `set -- $segment` of the script this replaces: quote characters stay on
    /// the words. PowerShell also splits argument arrays (`'run','dev'`) and
    /// drops the array and grouping brackets around them (`@('run','dev')`).
    /// Every word is a slice of `segment`, which [`tail`] relies on.
    fn words<'s>(&self, segment: &'s str) -> Vec<&'s str> {
        let separators: &[char] = match (self.dialect, self.windows) {
            (Dialect::PowerShell, _) => &[' ', '\t', '\n', '\r', ','],
            (Dialect::Posix, true) => &[' ', '\t', '\n', '\r'],
            (Dialect::Posix, false) => &[' ', '\t', '\n'],
        };
        segment
            .split(separators)
            .map(|word| match self.dialect {
                Dialect::PowerShell => word
                    .trim_start_matches("@(")
                    .trim_start_matches('(')
                    .trim_end_matches(')'),
                Dialect::Posix => word,
            })
            .filter(|word| !word.is_empty())
            .collect()
    }

    /// Whether `word` changes the shell's directory.
    fn is_cd(&self, word: &str) -> bool {
        if !self.windows {
            return word == "cd";
        }
        matches!(
            word.to_ascii_lowercase().as_str(),
            "cd" | "chdir" | "sl" | "set-location" | "pushd" | "push-location"
        )
    }

    /// Applies `cd ARGS` to `dir`. A directory that does not exist still moves
    /// `dir`, as the script this replaces did.
    fn change_dir(&self, args: &[&str], dir: &mut PathBuf) {
        let target = if self.windows {
            // `Set-Location -Path DIR`; a lone `-` is `cd -`.
            args.iter()
                .find(|arg| *arg == &"-" || !arg.starts_with('-'))
        } else {
            args.first()
        };
        let target = match target {
            Some(target) => strip_one_quote(target),
            None => match self.home {
                Some(home) => {
                    *dir = home.to_path_buf();
                    return;
                }
                None => return,
            },
        };
        let home_relative = target
            .strip_prefix("~/")
            .or_else(|| self.windows.then(|| target.strip_prefix("~\\")).flatten());
        let moved = match (target, home_relative, self.home) {
            ("-", _, _) => return,
            ("~", _, Some(home)) => home.to_path_buf(),
            (_, Some(rest), Some(home)) => home.join(rest),
            (target, _, _) => dir.join(native_path(target)),
        };
        *dir = lexical(&moved);
    }

    /// Unwraps the launcher at the start of `words`, if any, and says whether
    /// the command it starts is detached. `words` are slices of `segment`.
    fn launch<'w>(
        &self,
        segment: &'w str,
        mut words: &[&'w str],
        detached: bool,
        dir: &Path,
    ) -> Option<Launched<'w>> {
        let mut launch = Launch {
            detached,
            words: Vec::new(),
            dir: dir.to_path_buf(),
        };
        loop {
            let first = *words.first()?;
            match first {
                "setsid" => {
                    words = &words[1..];
                    while let Some(word) = words.first() {
                        if !word.starts_with('-') {
                            break;
                        }
                        if word.contains('f') {
                            launch.detached = true;
                        }
                        words = &words[1..];
                    }
                }
                "tmux" => {
                    if words
                        .iter()
                        .any(|word| matches!(*word, "new" | "new-session"))
                        && words.iter().any(|word| {
                            !word.starts_with("--") && word.starts_with('-') && word.contains('d')
                        })
                    {
                        launch.detached = true;
                    }
                }
                "screen" => {
                    let flags: String = words
                        .iter()
                        .filter(|word| word.starts_with('-'))
                        .copied()
                        .collect();
                    if flags.contains('d') && flags.contains('m') {
                        launch.detached = true;
                    }
                }
                _ if self.windows => {
                    let name = strip_quotes(first).to_ascii_lowercase();
                    match name.as_str() {
                        "start-process" | "saps" | "start" => {
                            return self
                                .start_process(&words[1..], launch)
                                .map(Launched::Command);
                        }
                        "start-job" | "sajb" | "start-threadjob" => {
                            launch.detached = true;
                            let rest = words.get(1).map_or("", |word| tail(segment, word));
                            if rest.contains('{') {
                                return Some(Launched::Script {
                                    text: brace_body(rest).to_owned(),
                                    detached: true,
                                    dir: launch.dir,
                                });
                            }
                            launch.words = script_block(&words[1..]);
                            return Some(Launched::Command(launch));
                        }
                        "invoke-expression" | "iex" => {
                            let mut args = &words[1..];
                            if args.first().is_some_and(|arg| {
                                arg.eq_ignore_ascii_case("-command")
                                    || arg.eq_ignore_ascii_case("-c")
                            }) {
                                args = &args[1..];
                            }
                            let text = strip_one_pair(tail(segment, args.first()?));
                            return Some(Launched::Script {
                                text: text.to_owned(),
                                detached: launch.detached,
                                dir: launch.dir,
                            });
                        }
                        // `cmd /c start ...`: judge the inner command.
                        "cmd" | "cmd.exe" => {
                            let rest = words[1..]
                                .iter()
                                .position(|word| {
                                    matches!(
                                        strip_quotes(word).to_ascii_lowercase().as_str(),
                                        "/c" | "//c" | "/k" | "//k"
                                    )
                                })
                                .map(|index| index + 2)?;
                            words = &words[rest..];
                            continue;
                        }
                        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => {
                            return powershell_command(segment, &words[1..], launch);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            launch.words = words.to_vec();
            return Some(Launched::Command(launch));
        }
    }

    /// `Start-Process` (or cmd's `start`): detached unless `-Wait` (and not
    /// `-Wait:$false`) or `/wait`. Opening a URL starts a browser, not a
    /// server, so that passes. `-WorkingDirectory` and `/d` move the command.
    fn start_process<'w>(&self, args: &[&'w str], mut launch: Launch<'w>) -> Option<Launch<'w>> {
        let mut wait = false;
        let mut command = Vec::new();
        let mut args = args.iter();
        let move_to = |launch: &mut Launch<'_>, value: &str| {
            launch.dir = lexical(&launch.dir.join(native_path(strip_quotes(value))));
        };
        while let Some(&arg) = args.next() {
            let lower = arg.to_ascii_lowercase();
            if lower.starts_with('-') && lower.len() > 1 {
                let (name, inline) = match lower.split_once(':') {
                    Some((name, value)) => (name, Some(value)),
                    None => (lower.as_str(), None),
                };
                match name {
                    "-wait" => wait = !matches!(inline, Some("$false" | "false" | "0")),
                    "-filepath" | "-path" | "-pspath" | "-argumentlist" | "-args" => {
                        if inline.is_some() {
                            command.push(&arg[name.len() + 1..]);
                        }
                    }
                    "-workingdirectory" => {
                        let value = if inline.is_some() {
                            Some(&arg[name.len() + 1..])
                        } else {
                            args.next().copied()
                        };
                        if let Some(value) = value {
                            move_to(&mut launch, value);
                        }
                    }
                    _ if START_PROCESS_VALUES.contains(&name) && inline.is_none() => {
                        args.next();
                    }
                    _ => {}
                }
                continue;
            }
            // cmd's `start` switches; `/d DIR` takes a value.
            if lower.len() > 1
                && lower.starts_with('/')
                && lower[1..].chars().all(|c| c.is_ascii_alphabetic())
                && lower.len() <= 12
            {
                match lower.as_str() {
                    "/wait" => wait = true,
                    "/d" => {
                        if let Some(value) = args.next() {
                            move_to(&mut launch, value);
                        }
                    }
                    "/node" | "/affinity" => {
                        args.next();
                    }
                    _ => {}
                }
                continue;
            }
            command.push(arg);
        }
        // cmd's `start "" program`: the first quoted argument is a window title.
        if command
            .first()
            .is_some_and(|first| strip_quotes(first).is_empty())
        {
            command.remove(0);
        }
        if command.first()?.contains("://") {
            return None;
        }
        launch.detached |= !wait;
        launch.words = command;
        Some(launch)
    }

    /// The program a word names, without directories, and on Windows without
    /// quotes, case and executable extensions.
    fn program(&self, word: &str) -> String {
        if !self.windows {
            return word.rsplit('/').next().unwrap_or(word).to_owned();
        }
        let name = strip_quotes(word)
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(word)
            .to_ascii_lowercase();
        for extension in [".exe", ".cmd", ".bat", ".ps1"] {
            if let Some(stem) = name.strip_suffix(extension) {
                return stem.to_owned();
            }
        }
        name
    }

    /// Words that name a long-running script or command: pnpm dev, npm start,
    /// docker compose up, or a server tool run without build or test.
    fn long_running(&self, words: &[&str]) -> bool {
        let long_word = |word: &str| {
            matches!(
                word,
                "dev"
                    | "start"
                    | "serve"
                    | "server"
                    | "preview"
                    | "watch"
                    | "--watch"
                    | "storybook"
                    | "runserver"
                    | "up"
                    | "http.server"
            ) || ["server.js", "server.mjs", "server.ts", "server.py"]
                .iter()
                .any(|suffix| word.ends_with(suffix))
        };
        let found = words.iter().any(|word| {
            let word = strip_one_quote(word);
            if self.windows {
                long_word(&word.to_ascii_lowercase())
            } else {
                long_word(word)
            }
        });
        if found {
            return true;
        }
        let Some(first) = words.first() else {
            return false;
        };
        SERVER_TOOLS.contains(&self.program(first).as_str())
            && !words
                .iter()
                .any(|word| matches!(*word, "build" | "test" | "lint" | "check"))
    }
}

/// What a launcher starts: a command to judge, or PowerShell text that runs
/// in its own scope (`pwsh -c`, `iex`, a job's script block).
enum Launched<'w> {
    Command(Launch<'w>),
    Script {
        text: String,
        detached: bool,
        /// Where the text starts; its `Set-Location` stays inside it.
        dir: PathBuf,
    },
}

/// A command a launcher starts, and whether it outlives the call.
struct Launch<'w> {
    detached: bool,
    words: Vec<&'w str>,
    /// Where it runs: `Start-Process -WorkingDirectory` can move it.
    dir: PathBuf,
}

/// `powershell`/`pwsh` arguments: the text after `-Command` (or the first
/// argument that is no option) runs as PowerShell; `-WorkingDirectory` moves it,
/// and the values of the other options are skipped. `-EncodedCommand` cannot be
/// read, and `-Command -` reads stdin, so both pass.
fn powershell_command<'w>(
    segment: &'w str,
    args: &[&'w str],
    mut launch: Launch<'w>,
) -> Option<Launched<'w>> {
    let mut index = 0;
    while let Some(&arg) = args.get(index) {
        index += 1;
        let lower = strip_quotes(arg).to_ascii_lowercase();
        let Some(name) = lower.strip_prefix('-').filter(|name| !name.is_empty()) else {
            index -= 1;
            break;
        };
        // `-ExecutionPolicy:Bypass` carries its value.
        let (name, inline) = match name.split_once(':') {
            Some((name, value)) => (name, Some(value)),
            None => (name, None),
        };
        let is = |full: &str, shortest: usize| name.len() >= shortest && full.starts_with(name);
        if name == "c" || is("command", 3) {
            break;
        }
        if name == "e" || name == "ec" || is("encodedcommand", 2) {
            return None;
        }
        if name == "wd" || is("workingdirectory", 2) {
            let value = match inline {
                Some(_) => arg.split_once(':').map_or("", |(_, value)| value),
                None => {
                    index += 1;
                    args.get(index - 1)?
                }
            };
            launch.dir = lexical(&launch.dir.join(native_path(strip_quotes(value))));
            continue;
        }
        let takes_value = matches!(name, "ep" | "ex" | "w" | "o" | "of" | "if" | "config")
            || is("executionpolicy", 3)
            || is("windowstyle", 3)
            || is("outputformat", 3)
            || is("inputformat", 2)
            || is("configurationname", 7)
            || is("configurationfile", 14)
            || is("settingsfile", 2)
            || is("custompipename", 2)
            || is("psconsolefile", 2);
        if takes_value && inline.is_none() {
            index += 1;
        }
    }
    let first = args.get(index)?;
    let text = strip_one_pair(tail(segment, first));
    if text == "-" {
        return None;
    }
    Some(Launched::Script {
        text: text.to_owned(),
        detached: launch.detached,
        dir: launch.dir,
    })
}

/// Programs that serve until stopped unless given a one-shot subcommand.
const SERVER_TOOLS: &[&str] = &[
    "vite",
    "next",
    "nuxt",
    "astro",
    "wrangler",
    "nodemon",
    "webpack-dev-server",
    "uvicorn",
    "gunicorn",
    "flask",
    "hugo",
    "http-server",
    "live-server",
];

/// Editors, IDEs and file openers: started in the background they open a file
/// such as `server.js` in a window, they don't serve it.
const EDITORS: &[&str] = &[
    "code",
    "code-insiders",
    "codium",
    "cursor",
    "windsurf",
    "zed",
    "subl",
    "notepad",
    "notepad++",
    "explorer",
    "devenv",
    "idea",
    "idea64",
    "webstorm",
    "webstorm64",
    "xdg-open",
    "open",
];

/// `Start-Process` parameters (with their aliases and the common parameters)
/// whose value is not part of the started command.
const START_PROCESS_VALUES: &[&str] = &[
    "-credential",
    "-redirectstandarderror",
    "-rse",
    "-redirectstandardinput",
    "-rsi",
    "-redirectstandardoutput",
    "-rso",
    "-windowstyle",
    "-verb",
    "-environment",
    "-erroraction",
    "-ea",
    "-warningaction",
    "-wa",
    "-informationaction",
    "-infa",
    "-errorvariable",
    "-ev",
    "-outvariable",
    "-ov",
    "-outbuffer",
    "-ob",
    "-pipelinevariable",
    "-pv",
];

/// The command words of a `Start-Job` without a script block, such as
/// `Start-Job -FilePath ./build.ps1`.
fn script_block<'w>(args: &[&'w str]) -> Vec<&'w str> {
    let mut words = Vec::new();
    let mut args = args.iter();
    while let Some(&arg) = args.next() {
        if arg.starts_with('-') {
            if matches!(
                arg.to_ascii_lowercase().as_str(),
                "-name" | "-initializationscript" | "-runas32" | "-psversion" | "-credential"
            ) {
                args.next();
            }
            continue;
        }
        let arg = arg.trim_matches(['{', '}']);
        if !arg.is_empty() {
            words.push(arg);
        }
    }
    words
}

/// Ends the current simple command, without grouping punctuation around it.
fn add(segment: &mut String, list: &mut Vec<String>) {
    let trimmed = segment
        .trim_start_matches([' ', '\t', '(', '{'])
        .trim_end_matches([' ', '\t', ')', '}']);
    if !trimmed.is_empty() {
        list.push(trimmed.to_owned());
    }
    segment.clear();
}

/// Ends a list of simple commands, all detached or none.
fn flush(
    detached: bool,
    segment: &mut String,
    list: &mut Vec<String>,
    segments: &mut Vec<(bool, String)>,
) {
    add(segment, list);
    segments.extend(list.drain(..).map(|command| (detached, command)));
}

/// Whether the text of an open double-quoted word starts like a Windows path:
/// a drive (`C:`), a UNC or rooted path (`\\`), or `.\` and `..\`.
fn windows_path(quoted: &str) -> bool {
    let bytes = quoted.as_bytes();
    matches!(bytes, [drive, b':', ..] if drive.is_ascii_alphabetic())
        || quoted.starts_with(r"\\")
        || matches!(quoted, "." | "..")
        || quoted.starts_with(r".\")
        || quoted.starts_with(r"..\")
}

/// The end (exclusive) of a PowerShell here-string whose `@` is just before
/// `start`: `@'` or `@"` ends its line, and the string closes at a line that
/// starts with `'@` or `"@`. Unclosed, it runs to the end of the text.
fn here_string_end(chars: &[char], start: usize) -> Option<usize> {
    let close = *chars.get(start).filter(|c| matches!(c, '\'' | '"'))?;
    let mut i = start + 1;
    while chars
        .get(i)
        .is_some_and(|c| *c == ' ' || *c == '\t' || *c == '\r')
    {
        i += 1;
    }
    if chars.get(i) != Some(&'\n') {
        return None;
    }
    while i < chars.len() {
        if chars[i] == '\n' && chars.get(i + 1) == Some(&close) && chars.get(i + 2) == Some(&'@') {
            return Some(i + 3);
        }
        i += 1;
    }
    Some(chars.len())
}

/// The index in `chars` of the first `pattern` at or after `from`.
fn find(chars: &[char], from: usize, pattern: &[char]) -> Option<usize> {
    chars
        .get(from..)?
        .windows(pattern.len())
        .position(|window| window == pattern)
        .map(|index| index + from)
}

/// The rest of `text` from `word` on, where `word` is a slice of `text` (as
/// every word from [`Check::words`] is); empty for any other `word`.
fn tail<'t>(text: &'t str, word: &str) -> &'t str {
    word.as_ptr()
        .addr()
        .checked_sub(text.as_ptr().addr())
        .and_then(|start| text.get(start..))
        .unwrap_or("")
}

/// The inside of the first `{ ... }` in `text`, quotes respected; to the end of
/// the text when the closing brace is missing, as when [`add`] trimmed it.
fn brace_body(text: &str) -> &str {
    let Some(open) = text.find('{') else {
        return "";
    };
    let body = &text[open + 1..];
    let mut depth = 0usize;
    let mut quote = None;
    for (index, c) in body.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '{') => depth += 1,
            (None, '}') if depth == 0 => return &body[..index],
            (None, '}') => depth -= 1,
            _ => {}
        }
    }
    body
}

/// `text` without one pair of surrounding quotes, as the shell that ran
/// `pwsh -c "..."` would have removed them; a missing closing quote is fine.
fn strip_one_pair(text: &str) -> &str {
    let text = text.trim();
    match text.chars().next() {
        Some(quote @ ('"' | '\'')) => {
            let inner = &text[1..];
            inner.strip_suffix(quote).unwrap_or(inner)
        }
        _ => text,
    }
}

/// `NAME=value`, a variable assignment before a command.
fn is_assignment(word: &str) -> bool {
    word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && word.contains('=')
}

/// Drops one quote character from each end, as `${word#[\"\']}` does.
fn strip_one_quote(word: &str) -> &str {
    let word = word.strip_prefix(['"', '\'']).unwrap_or(word);
    word.strip_suffix(['"', '\'']).unwrap_or(word)
}

/// Drops every surrounding quote character.
fn strip_quotes(word: &str) -> &str {
    word.trim_matches(['"', '\''])
}

/// Resolves `.` and `..` without touching the filesystem, like the shell's
/// logical `cd`.
fn lexical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    resolved.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    resolved.pop();
                }
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// A `cd` target as written.
#[cfg(not(windows))]
fn native_path(target: &str) -> PathBuf {
    PathBuf::from(target)
}

/// A `cd` target, with Git Bash's `/c/dir` read as `C:\dir`.
#[cfg(windows)]
fn native_path(target: &str) -> PathBuf {
    match target.as_bytes() {
        [b'/', drive, rest @ ..]
            if drive.is_ascii_alphabetic() && matches!(rest.first(), None | Some(b'/')) =>
        {
            let drive = char::from(*drive).to_ascii_uppercase();
            let rest = target[2..].trim_start_matches('/');
            PathBuf::from(format!("{drive}:\\{rest}"))
        }
        _ => PathBuf::from(target),
    }
}

/// devlanes exec and devlanes start only make sense inside a devlanes project.
fn in_project(dir: &Path) -> bool {
    dir.ancestors()
        .filter(|dir| !dir.as_os_str().is_empty())
        .any(|dir| dir.join("devlanes.yaml").is_file())
}
