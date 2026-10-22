//! Runs `env export --shell pwsh` scripts through Windows PowerShell, which every
//! Windows install has, decoding them with the code pages PowerShell may use for
//! a native command's output.

use std::{
    collections::BTreeMap,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine, prelude::BASE64_STANDARD};
use devlanes_cli::{ExportShell, export_environment};

#[test]
fn powershell_round_trips_exports_under_legacy_code_pages() {
    let values: BTreeMap<String, String> = BTreeMap::from([
        // UTF-8 for U+0151 ends in 0x91, a typographic quote in code page 1252.
        ("INJECT".into(), "\u{151}; Write-Output INJECTED; #".into()),
        ("SMART".into(), "a\u{2019}; Write-Output INJECTED; #".into()),
        ("PLAIN".into(), "it's $(Write-Output INJECTED)".into()),
        ("LINES".into(), "one\ntwo\tthree".into()),
        ("EMOJI".into(), "\u{1F600} caf\u{e9}".into()),
        ("CAF\u{c9}".into(), "x".into()),
        ("A*B".into(), "y".into()),
        ("ProgramFiles(x86)".into(), "C:\\Program Files (x86)".into()),
    ]);
    let exported = export_environment(&values, ExportShell::Pwsh);
    assert!(exported.skipped.is_empty(), "{:?}", exported.skipped);
    let dir = tempfile::tempdir().expect("temp dir");
    let script = dir.path().join("export.ps1");
    std::fs::write(&script, &exported.script).expect("write the export");
    let script = script.display().to_string().replace('\'', "''");
    let expected: Vec<String> = values
        .values()
        .map(|value| BASE64_STANDARD.encode(value))
        .collect();
    for code_page in [1252, 850, 65001] {
        // Progress records would arrive on stderr as CLIXML.
        let mut check = format!(
            "$ProgressPreference = 'SilentlyContinue'\n\
             $ErrorActionPreference = 'Stop'\n\
             $text = [Text.Encoding]::GetEncoding({code_page}).GetString([IO.File]::ReadAllBytes('{script}'))\n\
             Invoke-Expression $text\n"
        );
        // Values come back as Base64 so the check does not depend on the
        // console encoding either.
        for key in values.keys() {
            check.push_str(&format!(
                "[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes([string][Environment]::GetEnvironmentVariable([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}')))))\n",
                BASE64_STANDARD.encode(key)
            ));
        }
        let (success, stdout, stderr) = run_powershell(&check);
        assert!(success, "code page {code_page}: {stderr}");
        assert!(stderr.trim().is_empty(), "code page {code_page}: {stderr}");
        assert!(
            !stdout.contains("INJECTED"),
            "code page {code_page}: {stdout}"
        );
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines, expected, "code page {code_page}");
    }
}

/// Runs `script` in Windows PowerShell and returns whether it succeeded, its
/// stdout and its stderr; kills it after a minute so a hung shell fails the test
/// instead of the run.
fn run_powershell(script: &str) -> (bool, String, String) {
    let encoded: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let program = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"))
        .unwrap_or_else(|| PathBuf::from("powershell.exe"));
    let mut child = Command::new(program)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
        ])
        .arg(BASE64_STANDARD.encode(encoded))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start powershell.exe");
    let stdout = child.stdout.take().map(read_all);
    let stderr = child.stderr.take().map(read_all);
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait().expect("poll powershell.exe") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("powershell.exe did not finish within a minute");
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    let join = |reader: Option<thread::JoinHandle<String>>| {
        reader
            .map(|reader| reader.join().expect("reader thread"))
            .unwrap_or_default()
    };
    (status.success(), join(stdout), join(stderr))
}

/// Drains a pipe on its own thread so a full pipe cannot stall the child.
fn read_all(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}
