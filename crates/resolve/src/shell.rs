//! `config check` advice about the shell that runs `run:` strings.

use devlanes_config::{
    ConfigWarning, Extensions, InfraCommand, ProjectConfig, ReadyConfig, Run, Templated,
};
use devlanes_core::shell::Shell;

/// Why shell text with more than one line fails under the `cmd.exe` fallback,
/// for the unit's error and its `config check` warning.
pub(crate) const CMD_MULTILINE: &str = "has more than one line, but cmd.exe runs only the first: Git for Windows' sh.exe was not found. Install Git for Windows, then run `devlanes daemon restart`.";

/// Warns when `shell` is the `cmd.exe` fallback and the config has shell text:
/// a `run:` string or a `ready.cmd` check. One warning at the first such path
/// names how many there are, so a large config does not flood the output; each
/// text with more than one line, which fails to start under `cmd.exe`, gets a
/// warning of its own. Argv-form `run:` entries and `run_script` never use the
/// shell. Every message names its path.
pub fn shell_warnings<E: Extensions>(
    config: &ProjectConfig<E>,
    shell: &Shell,
) -> Vec<ConfigWarning> {
    if !shell.is_fallback() {
        return Vec::new();
    }
    let texts = shell_texts(config);
    let Some((first, _)) = texts.first() else {
        return Vec::new();
    };
    let subject = match texts.len() {
        1 => format!("`{first}` runs"),
        n => format!("`{first}` and {} more run", n - 1),
    };
    let mut warnings = vec![ConfigWarning {
        path: first.clone(),
        message: format!(
            "{subject} with cmd.exe: Git for Windows' sh.exe was not found, and POSIX syntax may fail there. Install Git for Windows, then run `devlanes daemon restart`."
        ),
    }];
    warnings.extend(
        texts
            .iter()
            .filter(|(_, text)| text.trim_end().contains(['\r', '\n']))
            .map(|(path, _)| ConfigWarning {
                path: path.clone(),
                message: format!("`{path}` {CMD_MULTILINE}"),
            }),
    );
    warnings
}

/// The text of a shell-string `run`, as written (an expression's template).
fn shell_text(run: Option<&Run>) -> Option<String> {
    match run {
        Some(Run::Shell(Templated::Literal(text) | Templated::Expression(text))) => {
            Some(text.clone())
        }
        _ => None,
    }
}

/// The text of a `ready.cmd` check; an argv `cmd` goes through the shell too,
/// as one line, so it counts with empty text.
fn ready_text(ready: &ReadyConfig) -> Option<String> {
    match ready {
        ReadyConfig::Check(check) => check
            .cmd
            .as_ref()
            .map(|cmd| shell_text(Some(cmd)).unwrap_or_default()),
        _ => None,
    }
}

/// Config paths whose command goes through the shell, with the shell text as
/// written, in declaration order.
fn shell_texts<E: Extensions>(config: &ProjectConfig<E>) -> Vec<(String, String)> {
    let mut texts = Vec::new();
    for (name, service) in &config.services {
        if let Some(text) = shell_text(service.run.as_ref()) {
            texts.push((format!("services.{name}.run"), text));
        }
        if let Some(text) = ready_text(&service.effective_ready()) {
            texts.push((format!("services.{name}.ready.cmd"), text));
        }
    }
    for (name, task) in &config.tasks {
        if let Some(text) = shell_text(task.run.as_ref()) {
            texts.push((format!("tasks.{name}.run"), text));
        }
    }
    for (phase, entries) in [
        ("setup", &config.infra.setup),
        ("teardown", &config.infra.teardown),
    ] {
        for (name, entry) in entries {
            match entry {
                InfraCommand::Command(run) => {
                    if let Some(text) = shell_text(Some(run)) {
                        texts.push((format!("infra.{phase}.{name}"), text));
                    }
                }
                InfraCommand::Object(options) => {
                    if let Some(text) = shell_text(options.run.as_ref()) {
                        texts.push((format!("infra.{phase}.{name}.run"), text));
                    }
                }
            }
        }
    }
    for (name, container) in &config.infra.containers {
        if let Some(text) = container.ready.as_ref().and_then(ready_text) {
            texts.push((format!("infra.containers.{name}.ready.cmd"), text));
        }
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Parses a project config for the tests.
    fn parse(yaml: &str) -> ProjectConfig {
        devlanes_config::parse_project(yaml, None).expect("valid config")
    }

    #[test]
    /// Only the cmd.exe fallback warns, once, at the first shell string, and
    /// names it and how many more there are.
    fn fallback_warns_once_at_the_first_shell_string() {
        let config = parse(
            "project: acme\nservices:\n  web:\n    run: [node, server.js]\n    ready:\n      cmd: curl -fsS http://127.0.0.1\n  api:\n    run: npm run dev\ntasks:\n  seed:\n    run: ./seed.sh\n",
        );
        assert!(shell_warnings(&config, &Shell::posix()).is_empty());
        let warnings = shell_warnings(&config, &Shell::cmd(Path::new("W")));
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].path, "services.web.ready.cmd");
        assert_eq!(
            warnings[0].message,
            "`services.web.ready.cmd` and 2 more run with cmd.exe: Git for Windows' sh.exe was not found, and POSIX syntax may fail there. Install Git for Windows, then run `devlanes daemon restart`."
        );
        let one = parse("project: acme\ntasks:\n  seed:\n    run: ./seed.sh\n");
        let warnings = shell_warnings(&one, &Shell::cmd(Path::new("W")));
        assert!(
            warnings[0]
                .message
                .starts_with("`tasks.seed.run` runs with cmd.exe: "),
            "{}",
            warnings[0].message
        );
        let argv_only = parse("project: acme\nservices:\n  web:\n    run: [node, server.js]\n");
        assert!(shell_warnings(&argv_only, &Shell::cmd(Path::new("W"))).is_empty());
    }

    #[test]
    /// Shell text with several lines gets a warning of its own under cmd.exe;
    /// a trailing line break from a YAML block does not count.
    fn fallback_warns_about_multi_line_text() {
        let config = parse(
            "project: acme\nservices:\n  web:\n    run: |\n      npm ci\n      npm run dev\n    ready:\n      cmd: |\n        curl -fsS http://127.0.0.1\ninfra:\n  setup:\n    migrate: \"a\\r\\nb\"\n",
        );
        assert!(shell_warnings(&config, &Shell::posix()).is_empty());
        let warnings = shell_warnings(&config, &Shell::cmd(Path::new("W")));
        let paths: Vec<_> = warnings.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "services.web.run",
                "services.web.run",
                "infra.setup.migrate"
            ]
        );
        assert_eq!(
            warnings[1].message,
            "`services.web.run` has more than one line, but cmd.exe runs only the first: Git for Windows' sh.exe was not found. Install Git for Windows, then run `devlanes daemon restart`."
        );
    }
}
