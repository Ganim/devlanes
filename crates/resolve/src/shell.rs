//! `config check` advice about the shell that runs `run:` strings.

use devlanes_config::{ConfigWarning, Extensions, InfraCommand, ProjectConfig, ReadyConfig, Run};
use devlanes_core::shell::Shell;

/// Warns when `shell` is the `cmd.exe` fallback and the config has shell text:
/// a `run:` string or a `ready.cmd` check. One warning at the first such path
/// names how many there are, so a large config does not flood the output.
/// Argv-form `run:` entries and `run_script` never use the shell.
pub fn shell_warnings<E: Extensions>(
    config: &ProjectConfig<E>,
    shell: &Shell,
) -> Vec<ConfigWarning> {
    if !shell.is_fallback() {
        return Vec::new();
    }
    let paths = shell_paths(config);
    let Some(first) = paths.first() else {
        return Vec::new();
    };
    let more = match paths.len() {
        1 => String::new(),
        n => format!(" (and {} more)", n - 1),
    };
    vec![ConfigWarning {
        path: first.clone(),
        message: format!(
            "runs with cmd.exe{more} because Git for Windows' sh.exe was not found; run strings and ready commands use POSIX shell syntax that cmd.exe may not understand. Install Git for Windows"
        ),
    }]
}

/// Config paths whose command goes through the shell, in declaration order.
fn shell_paths<E: Extensions>(config: &ProjectConfig<E>) -> Vec<String> {
    let mut paths = Vec::new();
    let ready_cmd =
        |ready: &ReadyConfig| matches!(ready, ReadyConfig::Check(check) if check.cmd.is_some());
    for (name, service) in &config.services {
        if matches!(service.run, Some(Run::Shell(_))) {
            paths.push(format!("services.{name}.run"));
        }
        if ready_cmd(&service.effective_ready()) {
            paths.push(format!("services.{name}.ready.cmd"));
        }
    }
    for (name, task) in &config.tasks {
        if matches!(task.run, Some(Run::Shell(_))) {
            paths.push(format!("tasks.{name}.run"));
        }
    }
    for (phase, entries) in [
        ("setup", &config.infra.setup),
        ("teardown", &config.infra.teardown),
    ] {
        for (name, entry) in entries {
            match entry {
                InfraCommand::Command(Run::Shell(_)) => {
                    paths.push(format!("infra.{phase}.{name}"));
                }
                InfraCommand::Object(options) if matches!(options.run, Some(Run::Shell(_))) => {
                    paths.push(format!("infra.{phase}.{name}.run"));
                }
                _ => {}
            }
        }
    }
    for (name, container) in &config.infra.containers {
        if container.ready.as_ref().is_some_and(ready_cmd) {
            paths.push(format!("infra.containers.{name}.ready.cmd"));
        }
    }
    paths
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
    /// Only the cmd.exe fallback warns, once, at the first shell string.
    fn fallback_warns_once_at_the_first_shell_string() {
        let config = parse(
            "project: acme\nservices:\n  web:\n    run: [node, server.js]\n    ready:\n      cmd: curl -fsS http://127.0.0.1\n  api:\n    run: npm run dev\ntasks:\n  seed:\n    run: ./seed.sh\n",
        );
        assert!(shell_warnings(&config, &Shell::posix()).is_empty());
        let warnings = shell_warnings(&config, &Shell::cmd(Path::new("W")));
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].path, "services.web.ready.cmd");
        assert!(
            warnings[0].message.contains("(and 2 more)"),
            "{}",
            warnings[0].message
        );
        let argv_only = parse("project: acme\nservices:\n  web:\n    run: [node, server.js]\n");
        assert!(shell_warnings(&argv_only, &Shell::cmd(Path::new("W"))).is_empty());
    }
}
