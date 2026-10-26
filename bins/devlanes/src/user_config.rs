//! The user config (`config.yaml` in the devlanes home) as CLI commands read it.
//! A missing file means every default. Commands whose effect depends on its
//! settings (certificate trust, the OS resolver file, the lane data folder of
//! `daemon reset-state`) refuse an invalid file with a `config_error`, and so
//! does the daemon, which then does not start (`daemon restart` checks first);
//! doctor reads it leniently and lists every problem; display settings (TUI
//! look, the tab's progress bar) keep every valid key after a warning.
//! `config get|set|unset` read and edit the file here, without the daemon.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use devlanes_cli::{Hint, OutputMode, Rendered, Runtime};
use devlanes_client::Paths;
use devlanes_config::{Applies, ConfigError, UserConfig};
use devlanes_proto::RpcError;
use serde_json::{Value, json};

use crate::output::emit;

/// `config.yaml` in the devlanes home.
pub fn file(paths: &Paths) -> PathBuf {
    paths.home.join("config.yaml")
}

/// The whole user config, or its first problem as a `config_error` naming the
/// file and line; a missing file is every default.
pub async fn load(paths: &Paths) -> Result<UserConfig, RpcError> {
    let path = file(paths);
    tokio::task::spawn_blocking(move || load_file(&path))
        .await
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?
}

/// The user config with every valid key kept: a problem drops only the key it is
/// about. For reports such as doctor, which list the problems themselves.
pub async fn lenient(paths: &Paths) -> UserConfig {
    let path = file(paths);
    tokio::task::spawn_blocking(move || lenient_file(&path).0)
        .await
        .unwrap_or_default()
}

/// The user config for settings that only change how output looks or whether
/// the TUI opens. Read once per process, leniently, so one bad key does not reset
/// the others; its problems go to stderr once as warnings.
/// Blocking on the first call.
pub fn display(paths: &Paths) -> &'static UserConfig {
    static DISPLAY: OnceLock<UserConfig> = OnceLock::new();
    DISPLAY.get_or_init(|| {
        let (config, problems) = lenient_file(&file(paths));
        emit(&Rendered {
            stderr: warnings(&problems),
            ..Rendered::default()
        });
        config
    })
}

/// "Edit the file at its line" for a `config_error` about the user config, in
/// place of the generic `config check` hint, which only checks project files.
pub fn hint(error: &RpcError) -> Option<Hint> {
    hint_for_file(error, &file(&Paths::discover().ok()?))
}

/// [`hint`] for the user config at `path`.
fn hint_for_file(error: &RpcError, path: &Path) -> Option<Hint> {
    let file = error.data.get("file").and_then(Value::as_str)?;
    if error.code != "config_error" || Path::new(file) != path {
        return None;
    }
    Some(Hint(match error.data.get("line").and_then(Value::as_u64) {
        Some(line) if line > 0 => format!("edit {file} at line {line}, then run this again"),
        _ => format!("edit {file}, then run this again"),
    }))
}

/// [`load`] on `path`. Blocking.
fn load_file(path: &Path) -> Result<UserConfig, RpcError> {
    if !path.exists() {
        return Ok(UserConfig::default());
    }
    devlanes_config::load_user(path).map_err(|error| config_error(&error))
}

/// The lenient load of `path` with its problems; a missing file has none. Blocking.
fn lenient_file(path: &Path) -> (UserConfig, Vec<ConfigError>) {
    if !path.exists() {
        return (UserConfig::default(), Vec::new());
    }
    devlanes_config::load_user_lenient(path)
}

/// The daemon's startup failure for an invalid user config, one line that
/// `devlanes_client::user_config_startup_error` turns back into this `config_error`.
pub fn startup_reason(error: &ConfigError) -> String {
    format!(
        "{}: {}",
        devlanes_client::USER_CONFIG_INVALID,
        config_error(error).data
    )
}

/// A user config problem in the daemon's `config_error` shape, so every client
/// words it as `<file> is invalid at line N, column M: <message>`.
fn config_error(error: &ConfigError) -> RpcError {
    RpcError::new(
        "config_error",
        json!({
            "message": error.message,
            "file": error.location.file,
            "line": error.location.line,
            "column": error.location.column,
        }),
        1,
    )
}

/// One `warning:` line per problem, worded like a `config_error`.
fn warnings(problems: &[ConfigError]) -> String {
    problems
        .iter()
        .map(|problem| {
            let location = &problem.location;
            format!(
                "warning: {} is invalid at line {}, column {}: {}\n",
                location.file, location.line, location.column, problem.message
            )
        })
        .collect()
}

/// `config get [KEY]`: the effective value of one setting or section, or of
/// the whole user config, defaults included. Read like the display settings,
/// so a broken file still shows what devlanes uses and its problems are
/// warnings.
pub async fn get(runtime: &Runtime, paths: &Paths, key: Option<&str>) -> Result<u8, RpcError> {
    if let Some(key) = key {
        devlanes_config::check_user_key(key).map_err(|error| unknown_setting(key, &error))?;
    }
    let shared = paths.clone();
    let config = blocking(move || display(&shared).clone()).await?;
    let value = match key {
        Some(key) => devlanes_config::user_setting(&config, key)
            .map_err(|error| unknown_setting(key, &error))?,
        None => devlanes_config::effective_user_value(&config),
    };
    let stdout = if runtime.mode == OutputMode::Json {
        let mut data = json!({"file": file(paths), "value": value});
        if let Some(key) = key {
            data["key"] = json!(key);
        }
        format!("{data}\n")
    } else if runtime.quiet {
        String::new()
    } else {
        devlanes_config::to_yaml(&value)
            .map_err(|error| RpcError::new("render_failed", json!({"message": error.message}), 1))?
    };
    emit(&Rendered {
        stdout,
        ..Rendered::default()
    });
    Ok(0)
}

/// `config set KEY VALUE`: writes `value`, read as YAML, to the user config,
/// keeping the file's comments. A value that does not parse or validate
/// leaves the file as it is.
pub async fn set(runtime: &Runtime, paths: &Paths, key: &str, value: &str) -> Result<u8, RpcError> {
    devlanes_config::check_user_key(key).map_err(|error| unknown_setting(key, &error))?;
    let path = file(paths);
    let value = devlanes_config::parse_setting_value(value)
        .map_err(|error| invalid_setting(&path, &format!("{key}: {}", error.message)))?;
    let (target, owned_key, written) = (path.clone(), key.to_owned(), value.clone());
    blocking(move || {
        file_value(&target)?;
        devlanes_config::set_user_value(&target, &owned_key, &written)
            .map_err(|error| invalid_setting(&target, &error.message))
    })
    .await??;
    let text = format!(
        "set {key} to {} in {}; {}\n",
        shown(&value),
        path.display(),
        takes_effect(key)
    );
    report(
        runtime,
        text,
        json!({"file": path, "key": key, "value": value, "applies": applies_name(key)}),
    );
    Ok(0)
}

/// `config unset KEY`: removes the setting so its default applies again. A
/// key the schema does not know is still removed when the file has it, so a
/// misspelled setting can be taken out.
pub async fn unset(runtime: &Runtime, paths: &Paths, key: &str) -> Result<u8, RpcError> {
    let known = devlanes_config::check_user_key(key);
    let path = file(paths);
    let (target, owned_key) = (path.clone(), key.to_owned());
    let present = blocking(move || {
        let file = file_value(&target)?;
        let present = owned_key
            .split('.')
            .try_fold(&file, |node, segment| node.get(segment))
            .is_some();
        if present {
            devlanes_config::remove_user_value(&target, &owned_key)
                .map_err(|error| invalid_setting(&target, &error.message))?;
        }
        Ok::<_, RpcError>(present)
    })
    .await??;
    if let Err(error) = known
        && !present
    {
        return Err(unknown_setting(key, &error));
    }
    let text = if present {
        format!(
            "removed {key} from {}; its default {}\n",
            path.display(),
            takes_effect(key)
        )
    } else {
        format!(
            "{key} is not set in {}; its default applies\n",
            path.display()
        )
    };
    report(
        runtime,
        text,
        json!({"file": path, "key": key, "removed": present, "applies": applies_name(key)}),
    );
    Ok(0)
}

/// Prints `text`, or `data` as JSON; `--quiet` keeps only the JSON.
fn report(runtime: &Runtime, text: String, data: Value) {
    let stdout = match runtime.mode {
        OutputMode::Json => format!("{data}\n"),
        _ if runtime.quiet => String::new(),
        _ => text,
    };
    emit(&Rendered {
        stdout,
        ..Rendered::default()
    });
}

/// When a change to `key` takes effect, as the end of a sentence. A running
/// TUI does not reread the file, so the colors and glyphs its options page
/// previews (`Applies::Now`) also wait for the next command here.
fn takes_effect(key: &str) -> &'static str {
    match devlanes_config::applies(key) {
        Applies::Now | Applies::NextCommand => "takes effect with the next devlanes command",
        Applies::Daemon => "takes effect after `devlanes daemon restart`",
    }
}

/// [`takes_effect`] for JSON output: `next_command` or `daemon`.
fn applies_name(key: &str) -> &'static str {
    match devlanes_config::applies(key) {
        Applies::Now | Applies::NextCommand => "next_command",
        Applies::Daemon => "daemon",
    }
}

/// A value on one line: strings as they are, lists and mappings in flow form.
fn shown(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The user config file as data, a missing one empty; a file that is not
/// YAML fails as a `config_error` at its line. Blocking.
fn file_value(path: &Path) -> Result<Value, RpcError> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(RpcError::new(
                "io",
                json!({"message": format!("reading {}: {error}", path.display())}),
                1,
            ));
        }
    };
    devlanes_config::parse_user_value(&source).map_err(|mut error| {
        error.location.file = path.display().to_string();
        config_error(&error)
    })
}

/// A key the user config has no setting or section for.
fn unknown_setting(key: &str, error: &ConfigError) -> RpcError {
    RpcError::new(
        "unknown_setting",
        json!({"key": key, "message": error.message}),
        2,
    )
}

/// A refused edit of the user config at `path`, which is left unchanged.
fn invalid_setting(path: &Path, message: &str) -> RpcError {
    RpcError::new(
        "invalid_setting",
        json!({"file": path, "message": message}),
        1,
    )
}

/// Runs file work off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))
}

#[cfg(test)]
mod tests {
    use devlanes_cli::{Cli, Runtime, clap::Parser};

    use super::*;

    /// A runtime for `--output <mode>`.
    fn runtime(mode: &str) -> Runtime {
        let cli = Cli::parse_from(["devlanes", "--output", mode]);
        Runtime::resolve(&cli, devlanes_cli::Signals::default()).expect("runtime")
    }

    /// A missing file is every default; one bad key fails the whole strict load
    /// with the file, line and message, while the lenient load keeps the rest.
    #[test]
    fn strict_load_refuses_what_lenient_load_drops() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        assert!(load_file(&path).is_ok());
        assert!(lenient_file(&path).1.is_empty());
        std::fs::write(&path, "tls:\n  ca: mkcert\ntui:\n  theme: neon\n").expect("write");
        let error = load_file(&path).expect_err("unknown theme");
        assert_eq!(error.code, "config_error");
        assert_eq!(error.data["file"], path.display().to_string());
        assert_eq!(error.data["line"], 4, "{error:?}");
        assert!(
            error.data["message"]
                .as_str()
                .is_some_and(|message| message.contains("neon")),
            "{error:?}"
        );
        // The lenient load drops the section holding the bad key, not its neighbours.
        let (config, problems) = lenient_file(&path);
        assert!(matches!(config.tls.ca, devlanes_config::CaKind::Mkcert));
        assert_eq!(problems.len(), 1);
        let warning = warnings(&problems);
        assert!(warning.starts_with("warning: "), "{warning}");
        assert!(warning.contains("is invalid at line 4"), "{warning}");
        assert_eq!(warning.lines().count(), 1);
    }

    /// The refusal renders like other config errors, on stderr in every output
    /// mode, with a hint to edit the user config rather than `config check`.
    #[test]
    fn config_errors_render_with_the_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, "hostnames:\n  tld: dev\n").expect("write");
        let error = load_file(&path).expect_err("public TLD");
        let hint = hint_for_file(&error, &path).expect("hint");
        assert_eq!(
            hint.0,
            format!("edit {} at line 2, then run this again", path.display())
        );
        let plain = runtime("plain").reporter().error(&error, Some(&hint));
        assert_eq!(plain.exit_code, 1);
        let first = plain.stderr.lines().next().unwrap_or_default();
        assert!(
            first.starts_with(&format!("error: {} is invalid at line 2", path.display())),
            "{}",
            plain.stderr
        );
        assert!(first.contains("hostnames.tld: `dev`"), "{}", plain.stderr);
        assert!(plain.stderr.contains("\nhint: edit "), "{}", plain.stderr);
        let json = runtime("json").reporter().error(&error, Some(&hint));
        assert_eq!((json.stdout, json.stderr), (String::new(), plain.stderr));
        // Another file's config error keeps its own hint.
        assert!(hint_for_file(&error, &temp.path().join("devlanes.yaml")).is_none());
    }
}
