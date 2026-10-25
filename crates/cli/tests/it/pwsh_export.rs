//! Runs `env export --shell pwsh` output in Windows PowerShell 5.1 and, when it
//! is on `PATH`, PowerShell 7, then compares that process's environment block
//! before and after the export.

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine, prelude::BASE64_STANDARD};
use devlanes_cli::{ExportShell, export_environment};

/// Prints every variable of the process as `<tag> <name> <value>`, both Base64
/// of UTF-8, so the check does not depend on the console code page. One line,
/// because `-Command -` reads its input line by line.
const DUMP: &str = "function Dump($tag) { foreach ($e in [Environment]::GetEnvironmentVariables().GetEnumerator()) { $tag + ' ' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes([string]$e.Key)) + ' ' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes([string]$e.Value)) } }";

/// Values and names that PowerShell would read as code, or that a code page
/// or line-based input would corrupt, if they were printed as they are.
fn values() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("QUOTES".into(), "it's \"double\" ''two'' '".into()),
        (
            "DOLLAR".into(),
            "$env:USERPROFILE $(Write-Output INJECTED) ${env:PATH} $null".into(),
        ),
        ("BACKTICK".into(), "a`nb `$x `\" trailing`".into()),
        ("LINES".into(), "one\ntwo\r\nthree\n".into()),
        ("ACCENT".into(), "caf\u{e9}".into()),
        ("JAPAN".into(), "\u{65e5}\u{672c}".into()),
        (
            "SMART".into(),
            "\u{2018}a\u{2019}; Write-Output INJECTED; #".into(),
        ),
        // The UTF-8 for U+0151 ends in 0x91, a typographic quote in code page 1252.
        ("CP1252".into(), "\u{151}'; Write-Output INJECTED; #".into()),
        ("SPACES".into(), "  padded  ".into()),
        ("EMPTY".into(), String::new()),
        // Long enough that a wrapping `Out-String` would split the line.
        ("LONG".into(), "\u{65e5}\u{672c} x".repeat(200)),
        ("NEXT-PUBLIC".into(), "dash".into()),
        // Kept on 7.5 and later like `EMPTY`, not deleted.
        ("EMPTY-DASH".into(), String::new()),
        ("A:B".into(), "colon".into()),
        ("A*[B]".into(), "wildcards".into()),
        ("1ST".into(), "digit".into()),
        ("A B".into(), "space".into()),
        ("A}B{C".into(), "braces".into()),
        ("CAF\u{c9}".into(), "accented name".into()),
    ])
}

#[test]
fn powershell_sets_exactly_the_exported_environment() {
    let values = values();
    let exported = export_environment(&values, ExportShell::Pwsh);
    assert!(exported.skipped.is_empty(), "{:?}", exported.skipped);
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("export.ps1");
    std::fs::write(&file, &exported.script).expect("write the export");
    // The documented form: a native command's output, decoded with the console
    // code page, then evaluated.
    let piped = format!(
        "cmd.exe /d /c type '{}' | Out-String | Invoke-Expression\n",
        file.display().to_string().replace('\'', "''")
    );
    let mut shells = vec![("powershell.exe", windows_powershell())];
    match on_path("pwsh.exe") {
        Some(pwsh) => shells.push(("pwsh", pwsh)),
        None => eprintln!("pwsh is not on PATH; only Windows PowerShell was checked"),
    }
    for (shell, program) in &shells {
        for (how, body) in [
            ("-Command -", exported.script.as_str()),
            ("Invoke-Expression", piped.as_str()),
        ] {
            let input = format!(
                "$ProgressPreference = 'SilentlyContinue'\n\
                 {DUMP}\n\
                 Dump BEFORE\n\
                 {body}\
                 Dump AFTER\n\
                 'VERSION ' + $PSVersionTable.PSVersion.Major + '.' + $PSVersionTable.PSVersion.Minor\n"
            );
            let (success, stdout, stderr) = run(program, &input);
            // `-Command -` keeps going and exits 0 after an error, so stderr
            // must be empty too.
            assert!(
                success && stderr.trim().is_empty(),
                "{shell} ({how}): {stderr}"
            );
            let dump = Dump::parse(&stdout);
            assert!(
                dump.other.is_empty(),
                "{shell} ({how}) printed {:?}",
                dump.other
            );
            let mut expected = dump.before.clone();
            for (name, value) in &values {
                // Before 7.5, PowerShell removes a variable assigned an empty value.
                if value.is_empty() && dump.version < (7, 5) {
                    expected.remove(name);
                } else {
                    expected.insert(name.clone(), value.clone());
                }
            }
            assert_eq!(dump.after, expected, "{shell} ({how})");
        }
    }
}

/// The environment before and after the export, the PowerShell version, and
/// any other line, which would be output of injected code.
struct Dump {
    before: BTreeMap<String, String>,
    after: BTreeMap<String, String>,
    version: (u32, u32),
    other: Vec<String>,
}

impl Dump {
    /// Reads the lines the test script prints.
    fn parse(stdout: &str) -> Self {
        let mut dump = Dump {
            before: BTreeMap::new(),
            after: BTreeMap::new(),
            version: (0, 0),
            other: Vec::new(),
        };
        let decode = |text: &str| {
            String::from_utf8(BASE64_STANDARD.decode(text).expect("Base64")).expect("UTF-8")
        };
        for line in stdout.lines() {
            let mut parts = line.splitn(3, ' ');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(tag @ ("BEFORE" | "AFTER")), Some(name), Some(value)) => {
                    let map = if tag == "BEFORE" {
                        &mut dump.before
                    } else {
                        &mut dump.after
                    };
                    map.insert(decode(name), decode(value));
                }
                (Some("VERSION"), Some(version), None) => {
                    let (major, minor) = version.split_once('.').expect("major.minor");
                    dump.version = (major.parse().expect("major"), minor.parse().expect("minor"));
                }
                _ => dump.other.push(line.to_owned()),
            }
        }
        assert!(
            !dump.before.is_empty() && dump.version != (0, 0),
            "{stdout}"
        );
        dump
    }
}

/// Windows PowerShell 5.1 from the system directory, which every Windows has.
fn windows_powershell() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"))
        .unwrap_or_else(|| PathBuf::from("powershell.exe"))
}

/// The first `name` on `PATH`. `symlink_metadata`, because the Microsoft Store
/// installs `pwsh.exe` as an app execution alias that `is_file` does not see.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| std::fs::symlink_metadata(path).is_ok())
}

/// Feeds `input` to `program -NoProfile -Command -` and returns whether it
/// exited 0, its stdout and its stderr. Kills it after a minute so a hung
/// shell fails the test instead of stalling the run.
fn run(program: &Path, input: &str) -> (bool, String, String) {
    let mut child = Command::new(program)
        .args(["-NoProfile", "-Command", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("start {}: {error}", program.display()));
    let mut stdin = child.stdin.take().expect("stdin");
    let input = input.to_owned();
    // On its own thread, so a shell that stops reading cannot block the test
    // past its deadline; dropping stdin ends the input.
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let stdout = child.stdout.take().map(read_all);
    let stderr = child.stderr.take().map(read_all);
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait().expect("poll the shell") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{} did not finish within a minute", program.display());
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    let _ = writer.join();
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
