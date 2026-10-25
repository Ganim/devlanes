//! The agent plugin's hook: which payloads each agent's hook reads with which
//! shell, and which commands the Bash and PowerShell readers block.

mod bash;
mod powershell;

use std::path::Path;

use devlanes_agent_hooks::{Agent, BLOCK_MESSAGE, Shell, ToolCall, Verdict, decide, deny_output};

/// A project at `<tmp>/project` with a `web` folder, and `<tmp>/other` outside it.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(dir.path().join("project/web")).expect("project");
    std::fs::create_dir_all(dir.path().join("other")).expect("other");
    std::fs::write(dir.path().join("project/devlanes.yaml"), "project: acme\n").expect("config");
    dir
}

/// A `PreToolUse` payload for `tool` running `command` in `cwd`.
fn payload(tool: &str, cwd: &Path, command: &str, background: bool) -> String {
    serde_json::json!({
        "session_id": "s",
        "cwd": cwd,
        "hook_event_name": "PreToolUse",
        "tool_name": tool,
        "tool_input": {"command": command, "run_in_background": background},
    })
    .to_string()
}

#[test]
fn each_agent_reads_its_tools_with_the_shell_that_runs_them() {
    for windows in [false, true] {
        assert_eq!(Agent::ClaudeCode.shell("Bash", windows), Some(Shell::Bash));
        assert_eq!(
            Agent::ClaudeCode.shell("PowerShell", windows),
            Some(Shell::PowerShell)
        );
        assert_eq!(Agent::ClaudeCode.shell("Edit", windows), None);
        assert_eq!(Agent::Codex.shell("PowerShell", windows), None);
        assert_eq!(Agent::Codex.shell("apply_patch", windows), None);
    }
    // Codex runs its shell commands in PowerShell on Windows and still names
    // the tool `Bash`.
    assert_eq!(Agent::Codex.shell("Bash", false), Some(Shell::Bash));
    assert_eq!(Agent::Codex.shell("Bash", true), Some(Shell::PowerShell));
    assert_eq!(Agent::from_name("claude-code"), Some(Agent::ClaudeCode));
    assert_eq!(Agent::from_name("codex"), Some(Agent::Codex));
    assert_eq!(Agent::from_name("cursor"), None);
}

#[test]
fn codex_commands_are_judged_in_the_hosts_shell() {
    let tmp = workspace();
    let project = tmp.path().join("project");
    let decide = |command: &str, windows: bool| {
        decide(
            &payload("Bash", &project, command, false),
            Agent::Codex,
            windows,
            Path::new("/unused"),
            None,
        )
    };
    assert_eq!(decide("pnpm dev &", false), Verdict::Block);
    assert_eq!(decide("pnpm dev &", true), Verdict::Block);
    assert_eq!(decide("Start-Process npm 'run dev'", true), Verdict::Block);
    assert_eq!(decide("Start-Process npm 'run dev'", false), Verdict::Allow);
    assert_eq!(decide("setsid -f pnpm dev", false), Verdict::Block);
    assert_eq!(decide("setsid -f pnpm dev", true), Verdict::Allow);
}

#[test]
fn reads_claude_code_and_codex_payloads() {
    let codex = r#"{"session_id":"s","turn_id":"t","transcript_path":null,"cwd":"/w","hook_event_name":"PreToolUse","model":"m","permission_mode":"default","tool_name":"Bash","tool_use_id":"u","tool_input":{"command":"pnpm dev &"}}"#;
    assert_eq!(
        ToolCall::parse(codex),
        Some(ToolCall {
            tool_name: "Bash".into(),
            command: "pnpm dev &".into(),
            run_in_background: false,
            cwd: Some("/w".into()),
        })
    );
    let claude = r#"{"session_id":"s","cwd":"","tool_name":"PowerShell","tool_input":{"command":"pnpm dev","description":"x","run_in_background":true}}"#;
    assert_eq!(
        ToolCall::parse(claude),
        Some(ToolCall {
            tool_name: "PowerShell".into(),
            command: "pnpm dev".into(),
            run_in_background: true,
            cwd: None,
        })
    );
    for payload in [
        "",
        "not json",
        "[]",
        r#"{"tool_name":"Bash","tool_input":{}}"#,
        r#"{"tool_name":"Bash","tool_input":{"command":["sh","-c","pnpm dev &"]}}"#,
        r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"pnpm dev &"}}"#,
    ] {
        assert_eq!(ToolCall::parse(payload), None, "{payload}");
    }
}

#[test]
fn falls_back_to_the_process_directory_without_cwd() {
    let tmp = workspace();
    for (tool, agent) in [
        ("Bash", Agent::ClaudeCode),
        ("PowerShell", Agent::ClaudeCode),
    ] {
        let payload =
            format!(r#"{{"tool_name":"{tool}","tool_input":{{"command":"pnpm dev &"}}}}"#);
        let verdict = |cwd: &Path| decide(&payload, agent, false, cwd, None);
        assert_eq!(
            verdict(&tmp.path().join("project")),
            Verdict::Block,
            "{tool}"
        );
        assert_eq!(verdict(&tmp.path().join("other")), Verdict::Allow, "{tool}");
    }
}

#[test]
fn ignores_tools_that_run_no_shell() {
    let tmp = workspace();
    let project = tmp.path().join("project");
    for agent in [Agent::ClaudeCode, Agent::Codex] {
        assert_eq!(
            decide(
                &payload("Write", &project, "pnpm dev &", false),
                agent,
                false,
                &project,
                None
            ),
            Verdict::Allow
        );
    }
}

#[test]
fn denies_with_the_pre_tool_use_answer() {
    let output: serde_json::Value = serde_json::from_str(&deny_output()).expect("JSON");
    assert_eq!(
        output,
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": BLOCK_MESSAGE,
            }
        })
    );
    assert!(BLOCK_MESSAGE.contains("devlanes exec --name NAME -- COMMAND"));
}
