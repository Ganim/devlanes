//! `devlanes __agent-hook` as the agent plugin runs it: JSON on stdin, exit 2 and
//! the message on stderr to block, `DEVLANES_BLOCK_BACKGROUND=0` to turn it off.

use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Runs the hook on `payload` and returns its exit status and stderr. The
/// child is killed if it outlives the deadline.
fn run_hook(payload: &str, home: &std::path::Path, opt_out: bool) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_devlanes"));
    command
        .arg("__agent-hook")
        .env("DEVLANES_HOME", home)
        .env_remove("DEVLANES_BLOCK_BACKGROUND")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if opt_out {
        command.env("DEVLANES_BLOCK_BACKGROUND", "0");
    }
    let mut child = command.spawn().expect("start devlanes");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(payload.as_bytes()).expect("write payload");
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the hook did not exit within 30 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    (status.code(), stderr)
}

#[test]
fn agent_hook_blocks_through_the_binary_and_can_be_turned_off() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("devlanes.yaml"), "project: acme\n").expect("config");
    let home = dir.path().join("home");
    for tool in ["Bash", "PowerShell"] {
        let payload = serde_json::json!({
            "cwd": dir.path(),
            "tool_name": tool,
            "tool_input": {"command": "pnpm dev &"},
        })
        .to_string();
        let (code, stderr) = run_hook(&payload, &home, false);
        assert_eq!(code, Some(2), "{tool}: {stderr}");
        assert!(
            stderr.contains("devlanes exec --name NAME"),
            "{tool}: {stderr}"
        );
        assert_eq!(run_hook(&payload, &home, true), (Some(0), String::new()));
    }
    let allowed = serde_json::json!({
        "cwd": dir.path(),
        "tool_name": "Bash",
        "tool_input": {"command": "pnpm dev"},
    })
    .to_string();
    assert_eq!(run_hook(&allowed, &home, false), (Some(0), String::new()));
    assert!(!home.exists(), "the hook must not create a devlanes home");
}
