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
    let powershell = payload["tool_name"].as_str() == Some("PowerShell");
    let mut dir = match payload["cwd"].as_str() {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => cwd.to_path_buf(),
    };
    let check = Check {
        powershell,
        windows: powershell || BASH_MAY_RUN_WINDOWS_COMMANDS,
        home,
    };
    let blocked = check
        .split(command)
        .into_iter()
        .any(|(tagged, segment)| check.blocks(&segment, tagged || background, &mut dir));
    if blocked {
        Verdict::Block
    } else {
        Verdict::Allow
    }
}

/// Git Bash ships a `start` that runs cmd's, and Codex sends PowerShell as the
/// `Bash` tool, so Windows `Bash` calls also get the Windows launchers and path
/// rules.
#[cfg(windows)]
const BASH_MAY_RUN_WINDOWS_COMMANDS: bool = true;

/// A Unix `Bash` call is POSIX shell only.
#[cfg(not(windows))]
const BASH_MAY_RUN_WINDOWS_COMMANDS: bool = false;

/// Settings for judging the simple commands of one call.
#[derive(Debug, Clone, Copy)]
struct Check<'a> {
    /// PowerShell quoting: `` ` `` escapes, `#` comments and `@' '@` quotes
    /// lines; otherwise `\` escapes.
    powershell: bool,
    /// Windows commands and paths apply: PowerShell and cmd launchers, `\`
    /// separators, `.exe`/`.cmd` names, case-insensitive commands, `$x =`
    /// assignments and the `&` call operator.
    windows: bool,
    home: Option<&'a Path>,
}

impl Check<'_> {
    /// Splits `command` into simple commands. Commands joined by `&&`, `||` or
    /// `|` share the flag of the operator that ends their list: `true` for a
    /// lone `&`. Operators inside quotes and redirections (`2>&1`, `&>`) don't
    /// split, and PowerShell comments are dropped.
    fn split(&self, command: &str) -> Vec<(bool, String)> {
        let escape = if self.powershell { '`' } else { '\\' };
        let chars: Vec<char> = command.chars().collect();
        let mut segments = Vec::new();
        let mut list = Vec::new();
        let mut segment = String::new();
        let mut quote = None;
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
            if c == escape {
                segment.push(c);
                if let Some(&escaped) = chars.get(i) {
                    segment.push(escaped);
                    i += 1;
                }
                continue;
            }
            if quote == Some('"') {
                segment.push(if c == '\n' { ' ' } else { c });
                if c == '"' {
                    quote = None;
                }
                continue;
            }
            if self.powershell {
                // A here-string is one word, so a `'` or `&` in a commit
                // message written with one neither quotes nor splits.
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
                // `#` starts a comment where a token may start: after blanks
                // and after `{` and `(`, which stay in the segment.
                if c == '#'
                    && segment
                        .chars()
                        .next_back()
                        .is_none_or(|c| c.is_whitespace() || matches!(c, '{' | '('))
                {
                    while chars.get(i).is_some_and(|&c| c != '\n') {
                        i += 1;
                    }
                    continue;
                }
            }
            if c == '\'' || c == '"' {
                quote = Some(c);
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
                // PowerShell's call operator, as in `& 'C:\tools\vite.cmd' dev`
                // or `Start-Job { & pnpm dev }`; in a POSIX shell a leading `&`
                // is a syntax error.
                '&' if self.windows
                    && segment
                        .trim_end()
                        .chars()
                        .next_back()
                        .is_none_or(|c| matches!(c, '{' | '(')) =>
                {
                    segment.push(c)
                }
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
                [first, ..]
                    if is_assignment(first)
                        || matches!(*first, "nohup" | "exec" | "command" | "time" | "env")
                        || (self.windows && *first == "&") =>
                {
                    words.remove(0);
                }
                // `$job = Start-Job { ... }`
                [first, "=", ..] if self.windows && first.starts_with('$') => {
                    words.drain(..2);
                }
                _ => break,
            }
        }
        let Some(&first) = words.first() else {
            return false;
        };
        if self.is_cd(first) {
            self.change_dir(&words[1..], dir);
            return false;
        }
        let Some(launch) = self.launch(&words, detached, dir) else {
            return false;
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
    /// drops the brackets around them (`@('run','dev')`), and on Windows the
    /// braces of script blocks go, so `Start-Job { pnpm dev }` reads as
    /// `Start-Job pnpm dev`.
    fn words<'s>(&self, segment: &'s str) -> Vec<&'s str> {
        let separators: &[char] = match (self.powershell, self.windows) {
            (true, _) => &[' ', '\t', '\n', '\r', ','],
            (false, true) => &[' ', '\t', '\n', '\r'],
            (false, false) => &[' ', '\t', '\n'],
        };
        segment
            .split(separators)
            .map(|word| match (self.powershell, self.windows) {
                (true, _) => word
                    .trim_matches(['{', '}'])
                    .trim_start_matches("@(")
                    .trim_start_matches('(')
                    .trim_end_matches(')'),
                (false, true) => word.trim_matches(['{', '}']),
                (false, false) => word,
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

    /// Unwraps the launchers at the start of `words`, if any, and says whether
    /// the command they start is detached. `None` when nothing runs that this
    /// hook can read, such as a URL or `pwsh -EncodedCommand`.
    fn launch<'w>(&self, mut words: &[&'w str], detached: bool, dir: &Path) -> Option<Launch<'w>> {
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
                _ if self.windows => match strip_quotes(first).to_ascii_lowercase().as_str() {
                    "start-process" | "saps" | "start" => {
                        return self.start(false, &words[1..], launch);
                    }
                    "start-job" | "sajb" | "start-threadjob" => {
                        return self.start(true, &words[1..], launch);
                    }
                    // `cmd /c start ...`, `pwsh -c "Start-Process ..."`: judge
                    // the command that follows.
                    "cmd" | "cmd.exe" | "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => {
                        let command = words.iter().position(|word| {
                            matches!(
                                strip_quotes(word).to_ascii_lowercase().as_str(),
                                "/c" | "//c" | "/k" | "//k" | "-c" | "-command"
                            )
                        })?;
                        words = &words[command + 1..];
                        continue;
                    }
                    _ => {}
                },
                _ => {}
            }
            launch.words = words.to_vec();
            return Some(launch);
        }
    }

    /// `Start-Process`, cmd's `start` (`job` false) and `Start-Job` (`job`
    /// true): detached unless `-Wait` or `/wait`, and a job always is.
    /// `-WorkingDirectory` and `/d` move the command. Opening a URL starts a
    /// browser, not a server, so that passes.
    fn start<'w>(&self, job: bool, args: &[&'w str], mut launch: Launch<'w>) -> Option<Launch<'w>> {
        let mut wait = false;
        let mut args = args.iter();
        while let Some(&arg) = args.next() {
            let lower = arg.to_ascii_lowercase();
            match lower.as_str() {
                "/wait" => wait = true,
                // `-Wait:$true` too.
                _ if lower.starts_with("-wait") => wait = true,
                "-workingdirectory" | "/d" => {
                    if let Some(value) = args.next() {
                        launch.dir = lexical(&launch.dir.join(native_path(strip_quotes(value))));
                    }
                }
                // Options whose value is not part of the started command.
                "-name" | "-windowstyle" | "-verb" | "-credential" => {
                    args.next();
                }
                _ if lower.starts_with("-redirectstandard") => {
                    args.next();
                }
                // Other options and cmd's switches; `-FilePath npm` keeps `npm`.
                _ if lower.len() > 1
                    && (lower.starts_with('-')
                        || (lower.starts_with('/')
                            && lower[1..].chars().all(|c| c.is_ascii_alphabetic()))) => {}
                // cmd's `start "" program` (an empty window title) and the call
                // operator of `Start-Job { & pnpm dev }`.
                _ if strip_quotes(arg).is_empty() || arg == "&" => {}
                _ => launch.words.push(arg),
            }
        }
        if launch.words.first()?.contains("://") {
            return None;
        }
        launch.detached |= job || !wait;
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

/// A command a launcher starts, and whether it outlives the call.
struct Launch<'w> {
    detached: bool,
    words: Vec<&'w str>,
    /// Where it runs: `Start-Process -WorkingDirectory` can move it.
    dir: PathBuf,
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
/// such as `server.js` in a window, they don't serve it. Some names count only
/// on the OS where they are editors: elsewhere `devenv` is Nix's (`devenv up`
/// starts services), and Linux `open` may be Debian's `openvt`, which runs the
/// command it is given.
const EDITORS: &[&str] = &[
    "code",
    "code-insiders",
    "codium",
    "cursor",
    "windsurf",
    "zed",
    "subl",
    "idea",
    "idea64",
    "webstorm",
    "webstorm64",
    "xdg-open",
    #[cfg(windows)]
    "notepad",
    #[cfg(windows)]
    "notepad++",
    #[cfg(windows)]
    "explorer",
    #[cfg(windows)]
    "devenv",
    #[cfg(target_os = "macos")]
    "open",
];

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
