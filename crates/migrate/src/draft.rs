//! The tool-neutral shape every source converts into before it becomes YAML.

use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

use crate::{Note, SourceKind};

/// What one source contributes: units, shared variables and source-level notes.
#[derive(Debug, Clone)]
pub(crate) struct Draft {
    pub kind: SourceKind,
    pub services: Vec<(String, Unit)>,
    pub tasks: Vec<(String, Unit)>,
    pub global_env: Vec<(String, String)>,
    pub notes: Vec<Note>,
}

impl Draft {
    /// An empty draft for `kind`.
    pub fn new(kind: SourceKind) -> Self {
        Self {
            kind,
            services: Vec::new(),
            tasks: Vec::new(),
            global_env: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Records a note that belongs to no single unit.
    pub fn note(&mut self, file: &str, message: impl Into<String>) {
        self.notes.push(Note {
            source: file.to_owned(),
            unit: None,
            message: message.into(),
        });
    }

    /// Whether the draft defines anything devlanes can run.
    pub fn is_empty(&self) -> bool {
        self.services.is_empty() && self.tasks.is_empty()
    }
}

/// What a unit runs. Text is devlanes template text: literal `${{` is already escaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    /// `run:` shell text.
    Shell(String),
    /// `run:` argument vector.
    Argv(Vec<String>),
    /// `run_script: <source>#<name>`.
    Script { source: String, name: String },
    /// Task `steps:`.
    Steps(Vec<String>),
}

impl Command {
    /// Shell text from a source, with literal `${{` escaped.
    pub fn shell(text: &str) -> Self {
        Self::Shell(escape(text))
    }

    /// An argument vector from a source, with literal `${{` escaped.
    pub fn argv(args: &[String]) -> Self {
        Self::Argv(args.iter().map(|arg| escape(arg)).collect())
    }
}

/// Lifecycle hook of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum When {
    Create,
    Start,
    Stop,
}

impl When {
    /// The `when:` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "lane.create",
            Self::Start => "lane.start",
            Self::Stop => "lane.stop",
        }
    }
}

/// A readiness check in devlanes terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Check {
    /// `tcp: true`: the service's own port.
    Tcp,
    /// `http: {path, status}` against the service's own port.
    Http { path: String, status: Option<u16> },
    /// `log:` regular expression.
    Log(String),
    /// `cmd:` retried until it exits 0.
    Cmd(Command),
    /// A TCP or HTTP probe on a literal port from the source; [`crate::ports`] turns it into
    /// one of the forms above once the service's own port is known.
    Probe { port: u16, http: Option<HttpProbe> },
}

/// The HTTP part of a [`Check::Probe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpProbe {
    /// The URL as the source wrote it, for a `curl` fallback.
    pub url: String,
    pub path: String,
    pub status: Option<u16>,
}

impl HttpProbe {
    /// Splits `http://host:port/path` into a probe; `None` for other schemes or hosts.
    pub fn parse(url: &str, status: Option<u16>) -> Option<(u16, Self)> {
        let rest = url.strip_prefix("http://")?;
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().ok()?),
            None => (authority, 80),
        };
        if !matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]") {
            return None;
        }
        Some((
            port,
            Self {
                url: url.to_owned(),
                path: path.to_owned(),
                status,
            },
        ))
    }
}

/// `ready:` with an optional overall timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ready {
    pub check: Check,
    pub timeout: Option<String>,
}

/// One service or task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unit {
    /// Source file the unit came from, for notes.
    pub file: String,
    pub run: Command,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub autostart: Option<bool>,
    pub replicas: Option<u32>,
    pub tags: Vec<String>,
    pub stop_signal: Option<String>,
    pub stop_timeout: Option<String>,
    pub ready: Option<Ready>,
    pub when: Option<When>,
    /// `after: services`: a lifecycle task that needs services running.
    pub after_services: bool,
    pub interactive: bool,
    /// Task `confirm:` text.
    pub confirm: Option<String>,
    /// A description from the source, written as a comment.
    pub description: Option<String>,
    /// Things to review, written as comments above the unit.
    pub notes: Vec<String>,
}

impl Unit {
    /// A unit that runs `run`, with everything else at the devlanes default.
    pub fn new(file: &str, run: Command) -> Self {
        Self {
            file: file.to_owned(),
            run,
            cwd: None,
            env: Vec::new(),
            autostart: None,
            replicas: None,
            tags: Vec::new(),
            stop_signal: None,
            stop_timeout: None,
            ready: None,
            when: None,
            after_services: false,
            interactive: false,
            confirm: None,
            description: None,
            notes: Vec::new(),
        }
    }

    /// Adds a note shown above the unit.
    pub fn note(&mut self, message: impl Into<String>) {
        self.notes.push(message.into());
    }

    /// Sets a variable, replacing an earlier value of the same name.
    pub fn set_env(&mut self, key: &str, value: String) {
        match self.env.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value,
            None => self.env.push((key.to_owned(), value)),
        }
    }

    /// The same settings, ignoring notes and the file they came from, so two sources that
    /// define an identical unit can be merged.
    pub fn same_settings(&self, other: &Self) -> bool {
        let strip = |unit: &Self| Self {
            file: String::new(),
            notes: Vec::new(),
            description: None,
            ..unit.clone()
        };
        strip(self) == strip(other)
    }
}

/// Escapes a literal `${{` so devlanes does not read it as an expression.
pub(crate) fn escape(text: &str) -> String {
    text.replace("${{", "${{ '${{' }}")
}

/// Hands out valid, unique unit names: `[a-z0-9][a-z0-9_-]*`, where `_` and `-` count as the
/// same character because devlanes expressions treat them as aliases. Names are also unique
/// as stored on disk ([`devlanes_core::paths::disk_name`], ignoring case), which config
/// validation requires.
#[derive(Debug, Default)]
pub(crate) struct Names {
    taken: HashSet<String>,
}

impl Names {
    /// A name for `raw`, plus a note when it had to change.
    pub fn claim(&mut self, raw: &str) -> (String, Option<String>) {
        let base = sanitize(raw);
        let mut name = base.clone();
        let mut n = 2;
        let key = |name: &str| {
            devlanes_core::paths::disk_name(name)
                .to_ascii_lowercase()
                .replace('_', "-")
        };
        while !self.taken.insert(key(&name)) {
            name = format!("{base}-{n}");
            n += 1;
        }
        let note = (name != raw).then(|| format!("renamed from {raw:?}"));
        (name, note)
    }
}

/// Lowercase ASCII letters, digits, `_` and `-`; anything else becomes `-`.
fn sanitize(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '_');
    if trimmed.is_empty() {
        "unit".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The project name devlanes accepts (`[a-z0-9][a-z0-9._-]*`), from a directory name. A
/// trailing `.` is dropped too: the name is valid, but Windows would drop the dot from its
/// directories, so devlanes would store it as `acme_`.
pub(crate) fn project_name(dir: &Path) -> String {
    let raw = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = String::new();
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out
        .trim_start_matches(['.', '_', '-'])
        .trim_end_matches(['-', '.']);
    if trimmed.is_empty() {
        "app".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// A working directory relative to the project, or why it cannot be one.
///
/// `base` is the directory relative paths in the source start from (relative to the
/// project). Absolute paths and `~` count only when they point inside the project, because
/// every lane runs in its own worktree.
pub(crate) fn relative_dir(
    project: &Path,
    base: &str,
    raw: &str,
    home: Option<&Path>,
) -> Result<Option<String>, String> {
    let expanded: PathBuf = if let Some(rest) = below_home(raw) {
        match home {
            Some(home) => home.join(rest),
            None => return Err(format!("{raw} depends on the home directory")),
        }
    } else {
        PathBuf::from(raw)
    };
    // `has_root`, not `is_absolute`: imported configs come from Unix tools, and
    // on Windows `/srv/x` has a root but no drive, yet still points outside.
    // On Unix the two are the same. The prefix check ignores case on Windows.
    let relative = if expanded.has_root() {
        match devlanes_core::paths::strip_path_prefix(&expanded, project) {
            Some(inside) => inside,
            None => {
                return Err(format!(
                    "{raw} is outside the project; every lane runs in its own worktree"
                ));
            }
        }
    } else {
        Path::new(base).join(expanded)
    };
    let mut parts: Vec<String> = Vec::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(format!("{raw} is outside the project"));
                }
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("/")))
}

/// The part of `raw` below the home directory for `~` and `~/...`.
#[cfg(unix)]
fn below_home(raw: &str) -> Option<&str> {
    raw.strip_prefix("~/").or((raw == "~").then_some(""))
}

/// The part of `raw` below the home directory for `~`, `~/...` and, as Windows
/// tools write it, `~\...`.
#[cfg(windows)]
fn below_home(raw: &str) -> Option<&str> {
    raw.strip_prefix("~/")
        .or_else(|| raw.strip_prefix("~\\"))
        .or((raw == "~").then_some(""))
}

/// Stop signals devlanes accepts.
const SIGNALS: [&str; 7] = [
    "SIGTERM", "SIGINT", "SIGHUP", "SIGQUIT", "SIGKILL", "SIGUSR1", "SIGUSR2",
];

/// `SIGINT` from `SIGINT`, `INT` or `sigint`, when devlanes accepts it.
pub(crate) fn signal_name(raw: &str) -> Option<&'static str> {
    let upper = raw.trim().to_ascii_uppercase();
    let full = if upper.starts_with("SIG") {
        upper
    } else {
        format!("SIG{upper}")
    };
    SIGNALS.iter().copied().find(|signal| *signal == full)
}

/// The signal for a Linux signal number, when devlanes accepts it.
pub(crate) fn signal_number(number: i64) -> Option<&'static str> {
    Some(match number {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        12 => "SIGUSR2",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// A devlanes duration for a number of milliseconds: `10s` when whole seconds, else `1600ms`.
pub(crate) fn duration_ms(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

/// A scalar from YAML, JSON or TOML as the string a process would see.
pub(crate) fn scalar_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// A duration such as `10s`, `500ms`, `2m` or `1h` when devlanes reads it the same way.
pub(crate) fn duration_text(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let digits = raw.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &raw[digits.len()..];
    (!digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && matches!(unit, "ms" | "s" | "m" | "h" | "d"))
    .then(|| raw.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Claimed names are unique as devlanes stores them: `_` and `-` are one
    /// character, and device names keep their spelling without colliding.
    fn claimed_names_do_not_collide_on_disk() {
        let mut names = Names::default();
        let claimed: Vec<String> = [
            "db:migrate",
            "db_migrate",
            "DB-Migrate",
            "aux",
            "AUX",
            "aux_",
        ]
        .into_iter()
        .map(|raw| names.claim(raw).0)
        .collect();
        assert_eq!(
            claimed,
            [
                "db-migrate",
                "db_migrate-2",
                "db-migrate-3",
                "aux",
                "aux-2",
                "aux-3"
            ]
        );
        let stored: HashSet<String> = claimed
            .iter()
            .map(|name| devlanes_core::paths::disk_name(name).to_ascii_lowercase())
            .collect();
        assert_eq!(stored.len(), claimed.len());
    }

    #[test]
    /// A trailing `.` or `-` is dropped from the project name.
    fn project_names_drop_a_trailing_dot() {
        assert_eq!(project_name(Path::new("acme.")), "acme");
        assert_eq!(project_name(Path::new("Acme App.-")), "acme-app");
        assert_eq!(project_name(Path::new("acme.web")), "acme.web");
        assert_eq!(project_name(Path::new("...")), "app");
    }

    #[test]
    /// `~` and absolute paths count only inside the project; relative ones
    /// start from `base`.
    fn relative_dirs_stay_inside_the_project() {
        let project = std::env::temp_dir().join("acme");
        let home = std::env::temp_dir();
        assert_eq!(
            relative_dir(&project, "apps", "web", None),
            Ok(Some("apps/web".to_owned()))
        );
        assert_eq!(
            relative_dir(&project, "", "~/acme/api", Some(&home)),
            Ok(Some("api".to_owned()))
        );
        let inside = project.join("srv").display().to_string();
        assert_eq!(
            relative_dir(&project, "", &inside, None),
            Ok(Some("srv".to_owned()))
        );
        assert!(relative_dir(&project, "", "~/other", Some(&home)).is_err());
        assert!(relative_dir(&project, "", "/srv/x", None).is_err());
        assert!(relative_dir(&project, "", "~/x", None).is_err());
    }

    #[cfg(windows)]
    #[test]
    /// On Windows `~\` is home-relative and the project prefix ignores case.
    fn windows_relative_dirs_ignore_case_and_take_backslash_home() {
        let project = Path::new(r"C:\Users\acme\code\Acme");
        let home = Path::new(r"C:\Users\acme");
        assert_eq!(
            relative_dir(project, "", r"~\code\acme\web", Some(home)),
            Ok(Some("web".to_owned()))
        );
        assert_eq!(
            relative_dir(project, "", r"c:\users\ACME\code\acme\api", None),
            Ok(Some("api".to_owned()))
        );
        assert!(relative_dir(project, "", r"D:\code\acme", None).is_err());
    }
}
