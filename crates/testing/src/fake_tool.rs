//! Installed copies of the `fake-tool` binary, which stand in for external programs
//! (`mise`, `direnv`, `mkcert`) where `#!/bin/sh` scripts cannot run, that
//! is on Windows.
//!
//! [`install`] copies the binary into a directory under the tool's name and writes
//! a [`Behaviour`] next to it (`mise.exe` reads `mise.exe.json`), so one directory
//! can hold several fakes that behave differently. Tests never build the binary:
//! it must exist next to the test binaries (`cargo build -p devlanes-testing`, or
//! any workspace build that includes this crate's tests).
//!
//! ```no_run
//! use devlanes_testing::fake_tool::{Behaviour, install};
//!
//! let bin = tempfile::tempdir()?;
//! let mise = install(
//!     bin.path(),
//!     "mise",
//!     &Behaviour::new()
//!         .args(["env", "--json"])
//!         .stdout_json(serde_json::json!({ "FROM_DIR": "{cwd}" })),
//! )?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::Error;

/// What an installed fake does on every run, in this order: check the arguments,
/// record them, sleep, print `stderr`, print `stdout` then `stdout_json`, exit.
///
/// Printed strings expand `{cwd}` to the canonical working directory (like
/// `pwd -P`; on Windows the verbatim `\\?\` form `Path::canonicalize` returns) and
/// `{env:NAME}` to a variable of the fake's environment, empty when unset. Other
/// braces stay as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Behaviour {
    /// Exact arguments expected; any others print `bad args: <args>` on stderr and
    /// exit 2 before anything else happens.
    pub args: Option<Vec<String>>,
    /// File that gets one JSON array of the arguments appended per run.
    pub record: Option<PathBuf>,
    /// Milliseconds to sleep, for timeout tests.
    pub sleep_ms: u64,
    pub stderr: String,
    pub stdout: String,
    /// Printed as compact JSON after `stdout`, with every string expanded.
    pub stdout_json: Option<serde_json::Value>,
    pub exit: u8,
}

impl Behaviour {
    /// A fake that accepts any arguments, prints nothing and exits 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requires exactly these arguments.
    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, args: I) -> Self {
        self.args = Some(args.into_iter().map(Into::into).collect());
        self
    }

    /// Appends the arguments of every run to `file`.
    pub fn record(mut self, file: impl Into<PathBuf>) -> Self {
        self.record = Some(file.into());
        self
    }

    /// Sleeps before printing anything.
    pub fn sleep(mut self, duration: Duration) -> Self {
        self.sleep_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        self
    }

    /// Text for stderr; add the trailing newline `echo` would print.
    pub fn stderr(mut self, text: impl Into<String>) -> Self {
        self.stderr = text.into();
        self
    }

    /// Text for stdout.
    pub fn stdout(mut self, text: impl Into<String>) -> Self {
        self.stdout = text.into();
        self
    }

    /// A JSON document for stdout; strings in it are expanded, then it is
    /// serialized, so expanded paths are escaped correctly.
    pub fn stdout_json(mut self, value: serde_json::Value) -> Self {
        self.stdout_json = Some(value);
        self
    }

    /// The exit code.
    pub fn exit(mut self, code: u8) -> Self {
        self.exit = code;
        self
    }
}

/// Copies `fake-tool` into `dir` as `name` (plus `.exe` on Windows), writes
/// `behaviour` next to it and returns the program path.
///
/// The binary is looked up as described in [`binary`]. Blocking filesystem work.
pub fn install(dir: &Path, name: &str, behaviour: &Behaviour) -> Result<PathBuf, Error> {
    let program = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(binary()?, &program)?;
    std::fs::write(sidecar(&program), serde_json::to_vec(behaviour)?)?;
    Ok(program)
}

/// The behaviour file a fake at `program` reads: its full file name plus
/// `.json` (`mise.exe.json`, `mise.old.json`), so names with dots of their own
/// keep distinct files.
pub fn sidecar(program: &Path) -> PathBuf {
    let mut name = program.as_os_str().to_owned();
    name.push(".json");
    PathBuf::from(name)
}

/// The prebuilt `fake-tool` in the target directory of the running test
/// (`<target>/<profile>/fake-tool`, next to the `deps` directory that holds test
/// binaries). Missing is an error that says how to build it; tests never run
/// cargo themselves.
pub fn binary() -> Result<PathBuf, Error> {
    let exe = std::env::current_exe()?;
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| Error::FakeTool(format!("no target directory above {}", exe.display())))?;
    let program = profile_dir.join(format!("fake-tool{}", std::env::consts::EXE_SUFFIX));
    if program.is_file() {
        Ok(program)
    } else {
        Err(Error::FakeTool(format!(
            "{} is missing; build it first with `cargo build -p devlanes-testing`",
            program.display()
        )))
    }
}

/// Expands `{cwd}` and `{env:NAME}` in `text`; `env` looks variables up.
pub fn expand(text: &str, cwd: &str, env: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some(after) = tail.strip_prefix("{cwd}") {
            out.push_str(cwd);
            rest = after;
        } else if let Some(name) = tail.strip_prefix("{env:")
            && let Some(end) = name.find('}')
        {
            out.push_str(&env(&name[..end]).unwrap_or_default());
            rest = &name[end + 1..];
        } else {
            out.push('{');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// [`expand`] applied to every string in a JSON value, keys excluded.
pub fn expand_json(
    value: &serde_json::Value,
    cwd: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => Value::String(expand(text, cwd, env)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| expand_json(v, cwd, env)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, v)| (key.clone(), expand_json(v, cwd, env)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known placeholders expand; unknown braces and unterminated ones stay.
    #[test]
    fn expands_placeholders() {
        let env = |name: &str| (name == "MARK").then(|| "seen".to_owned());
        assert_eq!(
            expand("{cwd}|{env:MARK}|{env:NONE}|{x}|{env:OPEN", "/w", &env),
            "/w|seen||{x}|{env:OPEN"
        );
        let value = serde_json::json!({"DIR": "{cwd}", "list": ["{env:MARK}"], "gone": null});
        assert_eq!(
            expand_json(&value, r"C:\w", &env),
            serde_json::json!({"DIR": r"C:\w", "list": ["seen"], "gone": null})
        );
    }

    /// Behaviours survive the sidecar round trip; missing fields take defaults.
    #[test]
    fn behaviour_round_trip() -> Result<(), serde_json::Error> {
        let behaviour = Behaviour::new()
            .args(["env", "--json"])
            .stderr("oops\n")
            .sleep(Duration::from_millis(5))
            .exit(3);
        let text = serde_json::to_string(&behaviour)?;
        assert_eq!(serde_json::from_str::<Behaviour>(&text)?, behaviour);
        assert_eq!(serde_json::from_str::<Behaviour>("{}")?, Behaviour::new());
        assert!(serde_json::from_str::<Behaviour>(r#"{"exit_code": 1}"#).is_err());
        Ok(())
    }
}
