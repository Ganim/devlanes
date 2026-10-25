//! `devlanes debug bundle`: a redacted `.tar.gz` for bug reports. It reads what
//! a running daemon reports and never spawns one, so a broken setup can still be
//! described.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use devlanes_cli::{OutputMode, Rendered, Runtime};
use devlanes_client::{Client, Context, Method, Paths};
use devlanes_proto::RpcError;
use serde_json::{Value, json};

use crate::output::emit;

/// Longest the `-- ARGS` run may take.
const RERUN_TIMEOUT: Duration = Duration::from_secs(300);

/// Name fragments of variables whose values never go into a bundle.
const SECRET_WORDS: [&str; 10] = [
    "TOKEN",
    "SECRET",
    "PASS",
    "KEY",
    "CREDENTIAL",
    "AUTH",
    "PRIVATE",
    "SESSION",
    "COOKIE",
    "DSN",
];

/// What replaces a hidden value.
const HIDDEN: &str = "[redacted]";

/// Collects, redacts and archives; prints the archive's path.
pub async fn run(
    runtime: &Runtime,
    to: Option<PathBuf>,
    log_lines: usize,
    args: Vec<String>,
    context: Context,
) -> Result<u8, RpcError> {
    let paths = Paths::discover().map_err(|error| io(error.to_string()))?;
    let redactor = Redactor::from_env(std::env::vars());
    let staging = tempfile::Builder::new()
        .prefix("devlanes-debug-")
        .tempdir()
        .map_err(|error| io(error.to_string()))?;
    let mut files: Vec<(&str, String)> = Vec::new();
    let context = Context {
        env: None,
        verbose: 0,
        ..context
    };
    let mut client = Client::connect(&paths).await;
    let mut ask = async |method: Method| -> Value {
        match &mut client {
            Ok(client) => match client.request(method, json!({}), context.clone()).await {
                Ok(data) => data,
                Err(devlanes_client::Error::Rpc(error)) => json!({"error": error}),
                Err(error) => json!({"error": {"message": error.to_string()}}),
            },
            Err(error) => {
                json!({"error": {"code": "daemon_not_connected", "message": error.to_string()}})
            }
        }
    };
    let daemon = ask(Method::DaemonStatus).await;
    let versions = json!({
        "client": {
            "version": env!("CARGO_PKG_VERSION"),
            "build_id": devlanes_version::BUILD_ID,
            "protocol": devlanes_proto::PROTOCOL_VERSION,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "daemon": daemon,
        "output": {"mode": runtime.mode.as_str(), "reason": runtime.reason},
    });
    files.push(("versions.json", redactor.json(&versions)));
    files.push(("doctor.json", redactor.json(&ask(Method::Doctor).await)));
    files.push((
        "config-check.json",
        redactor.json(&ask(Method::ConfigCheck).await),
    ));
    files.push(("status.json", redactor.json(&ask(Method::Status).await)));
    let log = tail_file(paths.logs.join("daemon.log"), log_lines).await;
    files.push(("daemon.log", redactor.text(&log)));
    if !args.is_empty() {
        files.push((
            "rerun.txt",
            redactor.text(&rerun(&args, &context.cwd).await),
        ));
    }
    files.push(("README.txt", readme(&args)));
    for (name, contents) in &files {
        let path = staging.path().join(name);
        let contents = contents.clone();
        blocking(move || std::fs::write(path, contents)).await?;
    }
    let archive = match to {
        Some(path) => absolute(&context.cwd, &path),
        None => std::env::temp_dir().join(format!(
            "devlanes-debug-{}.tar.gz",
            crate::daemon::utc_stamp(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs())
                    .unwrap_or_default()
            )
        )),
    };
    let status = tokio::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(staging.path())
        .args(files.iter().map(|(name, _)| *name))
        .status()
        .await
        .map_err(|error| io(format!("tar: {error}")))?;
    if !status.success() {
        return Err(io(format!("tar exited with {status}")));
    }
    if !runtime.quiet {
        let names: Vec<&str> = files.iter().map(|(name, _)| *name).collect();
        emit(&Rendered {
            stdout: if runtime.mode == OutputMode::Json {
                format!("{}\n", json!({"path": archive, "files": names}))
            } else {
                let shown = archive.display().to_string();
                // Clickable where the terminal shows links; `--to` may be relative.
                let linked = match (runtime.terminal.links, std::path::absolute(&archive)) {
                    (Some(links), Ok(path)) => links.file(&shown, &path, None, None),
                    _ => shown,
                };
                format!(
                    "wrote {linked}\n{}\n",
                    "secret-looking values are replaced; read the files before you send them"
                )
            },
            ..Rendered::default()
        });
    }
    Ok(0)
}

/// What the archive holds, for whoever opens it.
fn readme(args: &[String]) -> String {
    let mut text = String::from(
        "devlanes debug bundle\n\n\
versions.json      client and daemon versions, output mode\n\
doctor.json        devlanes doctor --json\n\
config-check.json  devlanes config check --json\n\
status.json        devlanes status --json\n\
daemon.log         the end of the daemon's log\n",
    );
    if !args.is_empty() {
        text.push_str(&format!(
            "rerun.txt          devlanes -vv {}: exit code, stdout, stderr\n",
            args.join(" ")
        ));
    }
    text.push_str(
        "\nValues of variables named like tokens, keys or passwords, passwords in URLs,\n\
env maps and the home directory were replaced before writing.\n",
    );
    text
}

/// Runs `devlanes -vv ARGS` in `cwd` and returns its exit code and output.
async fn rerun(args: &[String], cwd: &Path) -> String {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return format!("could not find devlanes: {error}\n"),
    };
    let mut command = tokio::process::Command::new(exe);
    command
        .arg("-vv")
        .args(args)
        .current_dir(cwd)
        // The TUI would wait for a person; this run is read, not watched.
        .env("DEVLANES_NO_UI", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut text = format!("$ devlanes -vv {}\n", args.join(" "));
    match tokio::time::timeout(RERUN_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => {
            text.push_str(&format!(
                "exit: {}\n\n--- stdout ---\n{}\n--- stderr ---\n{}",
                output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(Err(error)) => text.push_str(&format!("could not run: {error}\n")),
        Err(_) => text.push_str(&format!(
            "stopped after {}s without finishing\n",
            RERUN_TIMEOUT.as_secs()
        )),
    }
    text
}

/// The last `lines` lines of `path`, or a note when it cannot be read.
async fn tail_file(path: PathBuf, lines: usize) -> String {
    tokio::task::spawn_blocking(move || match std::fs::read(&path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let all: Vec<&str> = text.lines().collect();
            let mut tail = all[all.len().saturating_sub(lines)..].join("\n");
            tail.push('\n');
            tail
        }
        Err(error) => format!("{}: {error}\n", path.display()),
    })
    .await
    .unwrap_or_default()
}

/// `path` relative to `cwd` unless absolute.
fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

/// Runs file work off the executor.
async fn blocking(
    work: impl FnOnce() -> std::io::Result<()> + Send + 'static,
) -> Result<(), RpcError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| io(error.to_string()))?
        .map_err(|error| io(error.to_string()))
}

/// An `io` error with `message`.
fn io(message: String) -> RpcError {
    RpcError::new("io", json!({"message": message}), 1)
}

/// Whether a variable named `name` likely holds a secret.
fn secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_WORDS.iter().any(|word| upper.contains(word))
}

/// Replaces secrets in text and JSON: values of this shell's secret-looking
/// variables wherever they appear, `NAME=value` and `"name": value` pairs with
/// secret-looking names, values under an `env` object, URL passwords and the
/// home directory.
pub(crate) struct Redactor {
    /// Longest first, so a value containing another is replaced whole.
    values: Vec<String>,
    home: Option<String>,
}

impl Redactor {
    /// Learns the secret values of `vars` (the client's environment).
    pub(crate) fn from_env(vars: impl Iterator<Item = (String, String)>) -> Self {
        let mut home = None;
        let mut values: Vec<String> = Vec::new();
        for (name, value) in vars {
            if name == "HOME" && value.len() > 1 {
                home = Some(value.clone());
            }
            // Short values ("1", "yes") would blank out unrelated text.
            if secret_name(&name) && value.len() >= 6 {
                values.push(value);
            }
        }
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self { values, home }
    }

    /// Pretty JSON of `value`, redacted.
    pub(crate) fn json(&self, value: &Value) -> String {
        let mut value = value.clone();
        hide_json(&mut value, false);
        let text = serde_json::to_string_pretty(&value).unwrap_or_default();
        self.text(&format!("{text}\n"))
    }

    /// `text`, redacted; lines that are JSON documents are redacted as JSON too.
    pub(crate) fn text(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            let body = line.trim_end_matches('\n');
            match serde_json::from_str::<Value>(body) {
                Ok(mut value) if value.is_object() || value.is_array() => {
                    hide_json(&mut value, false);
                    out.push_str(&value.to_string());
                    out.push_str(&line[body.len()..]);
                }
                _ => out.push_str(line),
            }
        }
        for value in &self.values {
            out = out.replace(value.as_str(), HIDDEN);
        }
        let mut out = hide_assignments(&hide_url_passwords(&out));
        if let Some(home) = &self.home {
            out = out.replace(home.as_str(), "~");
        }
        out
    }
}

/// Hides string values under secret-looking keys, and every value of an `env`
/// object (`hide_all`).
fn hide_json(value: &mut Value, hide_all: bool) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if entry.is_string() && (hide_all || secret_name(key)) {
                    *entry = json!(HIDDEN);
                } else {
                    hide_json(entry, hide_all || key == "env");
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                hide_json(item, hide_all);
            }
        }
        Value::String(text) if hide_all => *text = HIDDEN.into(),
        _ => {}
    }
}

/// `scheme://user:password@host` with the password hidden.
fn hide_url_passwords(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        let (before, after) = rest.split_at(index + 3);
        out.push_str(before);
        let end = after
            .find(|c: char| c.is_whitespace() || "/\"'".contains(c))
            .unwrap_or(after.len());
        let authority = &after[..end];
        match (authority.rfind('@'), authority.find(':')) {
            (Some(at), Some(colon)) if colon < at => {
                out.push_str(&authority[..=colon]);
                out.push_str(HIDDEN);
                out.push_str(&authority[at..]);
            }
            _ => out.push_str(authority),
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// `NAME=value` with an upper-case, secret-looking NAME: the value hidden up to
/// whitespace or a quote. Lower-case names are diagnostic fields such as
/// `env_keys=A,B`, which list names only.
fn hide_assignments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('=') {
        let (before, after) = rest.split_at(index);
        let name_start = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        let name = &before[name_start..];
        out.push_str(before);
        out.push('=');
        let after = &after[1..];
        let upper = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if upper && secret_name(name) {
            let end = after
                .find(|c: char| c.is_whitespace() || "\"'".contains(c))
                .unwrap_or(after.len());
            if end > 0 {
                out.push_str(HIDDEN);
            }
            rest = &after[end..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A redactor that knows one secret and a home directory.
    fn redactor() -> Redactor {
        Redactor::from_env(
            [
                ("ACME_API_TOKEN", "tok-4711-very-secret"),
                ("HOME", "/Users/someone"),
                ("PATH", "/usr/bin:/bin"),
                ("CI_PASS", "1"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned())),
        )
    }

    #[test]
    /// Secret values, URL passwords, secret assignments, env objects and the home
    /// directory are hidden; diagnostics fields and ordinary values stay.
    fn hides_secrets_everywhere() {
        let redactor = redactor();
        let text = "\
   0.1s DEBUG daemon +3ms devlanes_daemon::lifecycle: spawning program=fixture-server env_keys=ACME_API_TOKEN,PORT\n\
ACME_API_TOKEN=tok-4711-very-secret\n\
GITHUB_TOKEN=ghp_other PORT=3000\n\
DATABASE_URL=postgres://postgres:hunter22@127.0.0.1:5432/app\n\
log line mentioning tok-4711-very-secret in passing\n\
{\"env\":{\"ACME_API_TOKEN\":\"tok-4711-very-secret\",\"PORT\":\"3000\"},\"api_key\":\"k-123456\",\"lane\":\"acme@0\"}\n\
cwd=/Users/someone/src/acme\n";
        let out = redactor.text(text);
        for secret in [
            "tok-4711-very-secret",
            "ghp_other",
            "hunter22",
            "k-123456",
            "/Users/someone",
        ] {
            assert!(!out.contains(secret), "{secret} leaked:\n{out}");
        }
        assert!(out.contains("env_keys=ACME_API_TOKEN,PORT"), "{out}");
        assert!(out.contains("PORT=3000"), "{out}");
        assert!(
            out.contains("postgres://postgres:[redacted]@127.0.0.1:5432/app"),
            "{out}"
        );
        assert!(out.contains("\"lane\":\"acme@0\""), "{out}");
        assert!(out.contains("cwd=~/src/acme"), "{out}");
        // Short values of secret-looking names are not hunted down everywhere.
        assert!(redactor.text("exit 1\n").contains("exit 1"));
    }

    #[test]
    /// JSON documents keep their shape with secret values replaced.
    fn json_keeps_its_shape() {
        let out = redactor().json(&json!({
            "daemon": {"version": "0.1.0", "token": "abcdefgh"},
            "items": [{"env": {"A": "b"}}],
        }));
        let parsed: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(parsed["daemon"]["version"], "0.1.0");
        assert_eq!(parsed["daemon"]["token"], HIDDEN);
        assert_eq!(parsed["items"][0]["env"]["A"], HIDDEN);
    }
}
