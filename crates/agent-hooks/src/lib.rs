//! The agent plugin's `PreToolUse` hook, run as `devlanes __agent-hook --agent
//! claude-code` from `agent-plugin/hooks/hooks.json` and as `devlanes
//! __agent-hook --agent codex` from `agent-plugin/hooks/codex.json`. It denies
//! shell commands that start a dev server in the background inside a devlanes
//! project and points the agent to `devlanes start` and `devlanes exec`.
//!
//! The plugin's commands end in `; exit 0` (cmd's `& exit 0` for Codex on
//! Windows), so a missing devlanes, or an older one whose usage error exits 2
//! (which both agents read as a block), lets every call pass. That also hides
//! this hook's exit status, so a block is a JSON `deny` on stdout with exit 0.

mod bash;
mod powershell;

use std::{
    ffi::OsString,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

/// First argument that runs the hook instead of the CLI. Hidden: the plugin's
/// hooks call it, people do not.
pub const ARG: &str = "__agent-hook";

/// What the agent reads when a call is blocked.
pub const BLOCK_MESSAGE: &str = "\
Blocked by the devlanes plugin: this starts a long-running process in the background, where nothing supervises it and its logs are lost.
Use instead:
  devlanes start [UNIT...]               services defined in devlanes.yaml
  devlanes exec --name NAME -- COMMAND   any other long-running command
Then check it with `devlanes wait NAME --timeout 180s` and `devlanes logs NAME --tail 100`.";

/// The coding agent whose hook runs, named by `--agent` in the plugin's hook
/// command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    /// Claude Code: the `Bash` tool runs Bash (Git Bash on Windows), the
    /// `PowerShell` tool runs PowerShell.
    ClaudeCode,
    /// Codex: every shell command is the `Bash` tool, run by the session's
    /// shell, which is PowerShell on Windows.
    Codex,
}

impl Agent {
    /// The agent for a `--agent` value: `claude-code` or `codex`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "claude-code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The shell that runs a call of `tool_name` for this agent, or `None`
    /// for tools that run no shell command. `windows` is the host the agent
    /// runs on.
    pub fn shell(self, tool_name: &str, windows: bool) -> Option<Shell> {
        match (self, tool_name) {
            (Self::ClaudeCode, "Bash") => Some(Shell::Bash),
            (Self::ClaudeCode, "PowerShell") => Some(Shell::PowerShell),
            (Self::Codex, "Bash") if windows => Some(Shell::PowerShell),
            (Self::Codex, "Bash") => Some(Shell::Bash),
            _ => None,
        }
    }
}

/// The shell language a command is read in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// POSIX sh and Bash.
    Bash,
    /// Windows PowerShell 5.1 and PowerShell 7.
    PowerShell,
}

impl Shell {
    /// Whether `command` starts a dev server in the background inside a
    /// devlanes project. `background` is the call's `run_in_background`;
    /// `cwd` is where the command starts and `home` resolves `cd ~`.
    pub fn blocks(self, command: &str, background: bool, cwd: &Path, home: Option<&Path>) -> bool {
        match self {
            Self::Bash => bash::blocks(command, background, cwd, home),
            Self::PowerShell => powershell::blocks(command, background, cwd, home),
        }
    }
}

/// The hook's decision for one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Block,
}

/// The fields of a `PreToolUse` payload the hook reads. Claude Code and Codex
/// send the same names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub tool_name: String,
    /// `tool_input.command`.
    pub command: String,
    /// `tool_input.run_in_background`, which only Claude Code sends.
    pub run_in_background: bool,
    /// The session's directory, when the payload names one.
    pub cwd: Option<PathBuf>,
}

impl ToolCall {
    /// Reads a `PreToolUse` payload. `None` for anything else: invalid JSON,
    /// another event, or a tool input without a string `command`.
    pub fn parse(payload: &str) -> Option<Self> {
        let payload: serde_json::Value = serde_json::from_str(payload).ok()?;
        if payload
            .get("hook_event_name")
            .is_some_and(|event| event != "PreToolUse")
        {
            return None;
        }
        let input = &payload["tool_input"];
        Some(Self {
            tool_name: payload["tool_name"].as_str()?.to_owned(),
            command: input["command"].as_str()?.to_owned(),
            run_in_background: input["run_in_background"].as_bool() == Some(true),
            cwd: payload["cwd"]
                .as_str()
                .filter(|cwd| !cwd.is_empty())
                .map(PathBuf::from),
        })
    }
}

/// Decides one `PreToolUse` payload from `agent`. `windows` is the host the
/// agent runs on, `cwd` stands in when the payload names no directory and
/// `home` resolves `cd ~`. A payload this hook cannot read is allowed.
///
/// A command is blocked when it runs in the background, looks like a dev
/// server, and runs in a directory, after any `cd` before it, inside a
/// project with `devlanes.yaml`.
///
/// ```
/// use std::path::Path;
/// use devlanes_agent_hooks::{Agent, Verdict, decide};
///
/// let project = tempfile::tempdir()?;
/// std::fs::write(project.path().join("devlanes.yaml"), "project: acme\n")?;
/// let payload = |command: &str| {
///     serde_json::json!({
///         "cwd": project.path(),
///         "tool_name": "Bash",
///         "tool_input": {"command": command},
///     })
///     .to_string()
/// };
/// let codex = |command: &str, windows| {
///     decide(&payload(command), Agent::Codex, windows, Path::new("."), None)
/// };
/// assert_eq!(codex("pnpm dev &", false), Verdict::Block);
/// assert_eq!(codex("pnpm dev", false), Verdict::Allow);
/// // Codex runs its `Bash` tool in PowerShell on Windows.
/// assert_eq!(codex("Start-Process npm 'run dev'", true), Verdict::Block);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn decide(
    payload: &str,
    agent: Agent,
    windows: bool,
    cwd: &Path,
    home: Option<&Path>,
) -> Verdict {
    let Some(call) = ToolCall::parse(payload) else {
        return Verdict::Allow;
    };
    let Some(shell) = agent.shell(&call.tool_name, windows) else {
        return Verdict::Allow;
    };
    let dir = call.cwd.as_deref().unwrap_or(cwd);
    if shell.blocks(&call.command, call.run_in_background, dir, home) {
        Verdict::Block
    } else {
        Verdict::Allow
    }
}

/// The `PreToolUse` answer that denies the call with [`BLOCK_MESSAGE`] as the
/// reason the agent sees. Claude Code and Codex read the same shape.
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

/// Runs the hook with the arguments after [`ARG`]: reads the tool call from
/// stdin and prints [`deny_output`] for a blocked one. Returns the exit
/// status, always 0. Arguments other than `--agent claude-code` or `--agent
/// codex` come from a newer plugin and allow the call; so does
/// `DEVLANES_BLOCK_BACKGROUND=0`.
pub fn run(args: impl IntoIterator<Item = OsString>) -> u8 {
    let mut input = Vec::new();
    // Drained first, so the agent never writes into a closed pipe.
    if std::io::stdin().read_to_end(&mut input).is_err() {
        return 0;
    }
    if std::env::var_os("DEVLANES_BLOCK_BACKGROUND").is_some_and(|value| value == "0") {
        return 0;
    }
    let args: Vec<OsString> = args.into_iter().collect();
    let agent = match args.as_slice() {
        [flag, name] if flag == "--agent" => name.to_str().and_then(Agent::from_name),
        _ => None,
    };
    let Some(agent) = agent else {
        return 0;
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = std::env::home_dir();
    let payload = String::from_utf8_lossy(&input);
    if decide(&payload, agent, cfg!(windows), &cwd, home.as_deref()) == Verdict::Block {
        let mut stdout = std::io::stdout().lock();
        // An agent that stopped reading gets no decision either way.
        let _ = writeln!(stdout, "{}", deny_output()).and_then(|()| stdout.flush());
    }
    0
}

/// `target` resolved against `dir`, with `~` as `home`, and `.` and `..`
/// resolved without touching the filesystem. A directory that does not exist
/// still counts.
fn moved(dir: &Path, target: &str, home: Option<&Path>) -> PathBuf {
    let path = match (target.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => dir.join(target),
    };
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

/// A command name without its directory, case or executable extension:
/// `C:\tools\Vite.CMD` is `vite`.
fn program(word: &str) -> String {
    let name = word
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

/// Whether `words`, started in the background in `dir`, is a dev server
/// inside a project. A backgrounded devlanes command, such as `devlanes logs
/// -f`, passes.
fn serves(words: &[&str], dir: &Path) -> bool {
    words
        .first()
        .is_some_and(|first| program(first) != "devlanes")
        && long_running(words)
        && in_project(dir)
}

/// Words that name a long-running script or command: `pnpm dev`, `npm start`,
/// `docker compose up`, or a server tool run without `build` or `test`.
fn long_running(words: &[&str]) -> bool {
    let listed = |list: &str, word: &str| list.split(' ').any(|item| item == word);
    let script = words.iter().any(|word| {
        listed(LONG_RUNNING_WORDS, word)
            || ["server.js", "server.mjs", "server.ts", "server.py"]
                .iter()
                .any(|suffix| word.ends_with(suffix))
    });
    script
        || words.first().is_some_and(|first| {
            listed(SERVER_TOOLS, &program(first))
                && !words
                    .iter()
                    .any(|word| listed("build test lint check", word))
        })
}

/// Arguments that name a dev script or a serving subcommand.
const LONG_RUNNING_WORDS: &str =
    "dev start serve server preview watch --watch storybook runserver up http.server";

/// Programs that serve until stopped unless given a one-shot subcommand.
const SERVER_TOOLS: &str = "vite next nuxt astro wrangler nodemon webpack-dev-server uvicorn \
                            gunicorn flask hugo http-server live-server";

/// devlanes exec and devlanes start only make sense inside a devlanes project.
fn in_project(dir: &Path) -> bool {
    dir.ancestors()
        .filter(|dir| !dir.as_os_str().is_empty())
        .any(|dir| dir.join("devlanes.yaml").is_file())
}
