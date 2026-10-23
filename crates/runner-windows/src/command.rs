//! Resolving a unit's program and building the command line Windows starts it with.
//!
//! `CreateProcessW` takes an application path and one command-line string, which
//! the child splits itself. This module builds both the way std's `Command` does
//! on Windows, except that bare names are looked up on the unit's `PATH` with
//! `PATHEXT` (std only appends `.exe`, so `npm.cmd` is never found), and that
//! Git's `sh.exe` gets the quoting its MSYS2 runtime expects.
//!
//! Everything here is string work plus file existence checks.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use devlanes_shell_windows::quote_msys;

/// Extensions `CreateProcessW` can start, in the default `PATHEXT` order. Other
/// `PATHEXT` entries (`.JS`, `.PS1`, ...) need a host program and are skipped.
const RUNNABLE_EXTENSIONS: [&str; 4] = [".com", ".exe", ".bat", ".cmd"];

/// Characters that never need quotes in a batch file argument; std's list. Every
/// other ASCII symbol is quoted, so `cmd.exe` cannot read it as an operator.
const BATCH_UNQUOTED: &str = r"#$*+-./:?@\_";

/// What `CreateProcessW` needs to start a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command {
    /// The executable to start (`lpApplicationName`): the resolved program, or
    /// `<System32>\cmd.exe` for a batch file.
    pub application: PathBuf,
    /// The whole command line (`lpCommandLine`), argv0 included.
    pub command_line: OsString,
}

/// How arguments are quoted on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quoting {
    /// The Microsoft C runtime's rules, which most programs use; std's quoting.
    Native,
    /// The MSYS2 runtime of Git's `sh.exe`: every argument in double quotes, so
    /// it is never glob-expanded, with `\` and `"` escaped by a backslash.
    Msys,
}

/// Why a command could not be built.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CommandError {
    /// No runnable file for `program`. `searched` lists the `PATH` directories
    /// tried for a bare name, or the one directory an explicit path names.
    #[error("{} not found", program.display())]
    NotFound {
        program: PathBuf,
        searched: Vec<PathBuf>,
    },
    /// A batch file argument `cmd.exe` would cut short (a line break); std
    /// refuses the same arguments.
    #[error("argument {0:?} cannot be passed to a batch file safely")]
    ArgumentNotSafeForBatch(String),
    /// Windows command lines cannot carry NUL characters.
    #[error("{0:?} contains a NUL character")]
    ContainsNul(String),
    /// Empty, ending in a path separator, relative to a drive's current
    /// directory (`C:tool`), or (for a batch file) containing a quote.
    #[error("{0:?} is not a usable program path")]
    InvalidProgram(String),
}

/// Resolves `program` for a child with environment `env` started in `cwd`, and
/// builds the command line that passes it `args` quoted per `quoting`. Batch
/// files run through `system\cmd.exe`, where `system` is the System32
/// directory. Checks the filesystem for candidates, so call it off async
/// executor threads.
pub(crate) fn build(
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
    system: &Path,
    quoting: Quoting,
) -> Result<Command, CommandError> {
    build_with(program, args, env, cwd, system, quoting, &Path::is_file)
}

/// [`build`] with the existence check passed in, so tests need no files.
fn build_with(
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
    system: &Path,
    quoting: Quoting,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<Command, CommandError> {
    for text in std::iter::once(program).chain(args.iter().map(String::as_str)) {
        if text.contains('\0') {
            return Err(CommandError::ContainsNul(text.to_owned()));
        }
    }
    let application = resolve(program, env, cwd, exists)?;
    let (application, argv0, arguments) = if is_batch_file(&application) {
        let script = application.to_string_lossy();
        (
            system.join("cmd.exe"),
            "cmd.exe".to_owned(),
            batch_arguments(&script, args)?,
        )
    } else {
        let quote = match quoting {
            Quoting::Native => quote,
            Quoting::Msys => quote_msys,
        };
        let quoted: Vec<String> = args.iter().map(|arg| quote(arg)).collect();
        let argv0 = match quoting {
            // File names cannot contain quotes, so there is nothing to escape.
            Quoting::Native => format!("\"{program}\""),
            Quoting::Msys => quote_msys(program),
        };
        (application, argv0, quoted.join(" "))
    };
    let command_line = if arguments.is_empty() {
        argv0
    } else {
        format!("{argv0} {arguments}")
    };
    Ok(Command {
        application,
        command_line: command_line.into(),
    })
}

/// The file `program` names: an explicit path from `cwd`, or a bare name from
/// the `PATH` directories, trying the name as given when it has an extension
/// and then each runnable `PATHEXT` extension, directory by directory.
fn resolve(
    program: &str,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<PathBuf, CommandError> {
    // `C:tool` is relative to drive C's current directory, which would be the
    // daemon's, not the unit's.
    if program.is_empty() || program.ends_with(['\\', '/']) || drive_relative(program) {
        return Err(CommandError::InvalidProgram(program.to_owned()));
    }
    let extensions = extensions(env);
    let has_extension = runnable_extension(file_name(program));
    if devlanes_core::paths::names_a_path(program) {
        let path = from_cwd(cwd, program);
        return candidates(path.clone(), has_extension, &extensions)
            .find(|candidate| exists(candidate))
            .ok_or_else(|| CommandError::NotFound {
                program: program.into(),
                searched: path.parent().map(Path::to_path_buf).into_iter().collect(),
            });
    }
    let dirs = search_path(env, cwd);
    dirs.iter()
        .flat_map(|dir| candidates(dir.join(program), has_extension, &extensions))
        .find(|candidate| exists(candidate))
        .ok_or_else(|| CommandError::NotFound {
            program: program.into(),
            searched: dirs,
        })
}

/// Whether `name` already ends in an extension `CreateProcessW` can start, after
/// the trailing dots and spaces Windows drops (`run.bat.` is `run.bat`). Others
/// (`tool.js`, `npm.`, which Windows reads as `npm`) get the `PATHEXT` extensions
/// appended instead of being tried as they are.
fn runnable_extension(name: &str) -> bool {
    let name = name.trim_end_matches(['.', ' ']);
    name.rfind('.').is_some_and(|dot| {
        RUNNABLE_EXTENSIONS
            .iter()
            .any(|runnable| name[dot..].eq_ignore_ascii_case(runnable))
    })
}

/// Files to try for `path`: as given when its name has a runnable extension, then
/// with each of `extensions` appended.
fn candidates(
    path: PathBuf,
    has_extension: bool,
    extensions: &[String],
) -> impl Iterator<Item = PathBuf> {
    let given = has_extension.then(|| path.clone());
    given
        .into_iter()
        .chain(extensions.iter().map(move |extension| {
            let mut candidate = path.as_os_str().to_owned();
            candidate.push(extension);
            PathBuf::from(candidate)
        }))
}

/// The runnable `PATHEXT` extensions in their order, lowercased; the default
/// list when `PATHEXT` is unset or names none of them.
fn extensions(env: &BTreeMap<String, String>) -> Vec<String> {
    let listed: Vec<String> = var(env, "PATHEXT")
        .unwrap_or_default()
        .split(';')
        .map(|extension| extension.trim().to_ascii_lowercase())
        .filter(|extension| RUNNABLE_EXTENSIONS.contains(&extension.as_str()))
        .collect();
    if listed.is_empty() {
        RUNNABLE_EXTENSIONS.map(str::to_owned).into()
    } else {
        listed
    }
}

/// The non-empty `PATH` directories, relative ones taken from `cwd` like the
/// child would. Split the way std's `env::split_paths` does on Windows: `;`
/// separates, and double quotes (dropped) protect a `;` inside an entry.
fn search_path(env: &BTreeMap<String, String>, cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut entry = String::new();
    let mut quoted = false;
    for character in var(env, "PATH").unwrap_or_default().chars().chain([';']) {
        match character {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                // A `C:dir` entry depends on the daemon's current directory on that
                // drive, so it is skipped rather than guessed.
                if !entry.is_empty() && !drive_relative(&entry) {
                    dirs.push(from_cwd(cwd, &entry));
                }
                entry.clear();
            }
            _ => entry.push(character),
        }
    }
    dirs
}

/// Looks `name` up ignoring ASCII case, as Windows does (`Path` is `PATH`); an
/// exact spelling wins when both exist.
fn var<'a>(env: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    env.get(name)
        .or_else(|| {
            env.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value)
        })
        .map(String::as_str)
}

/// Whether `path` starts with a drive letter and colon.
fn has_drive(path: &str) -> bool {
    matches!(path.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic())
}

/// Whether `path` is like `C:dir`: a drive without a separator after it.
fn drive_relative(path: &str) -> bool {
    has_drive(path) && !is_absolute(path)
}

/// Whether `path` is absolute by Windows rules: `C:\...` or a UNC/device path
/// (`\\server\share`). `\dir` and `C:dir` are relative (to a drive).
fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    let separator = |byte: &u8| *byte == b'\\' || *byte == b'/';
    (has_drive(path) && bytes.get(2).is_some_and(separator))
        || (bytes.first().is_some_and(separator) && bytes.get(1).is_some_and(separator))
}

/// `path` as is when absolute, else joined to `cwd`.
fn from_cwd(cwd: &Path, path: &str) -> PathBuf {
    if is_absolute(path) {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    }
}

/// The last component of `path`, split on either separator or a drive colon.
fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/', ':']).next().unwrap_or(path)
}

/// Whether `path` is a `.bat` or `.cmd` file, which `CreateProcessW` hands to
/// `cmd.exe`. Windows drops trailing dots and spaces from file names, so
/// `run.bat.` is a batch file too and must get batch escaping.
fn is_batch_file(path: &Path) -> bool {
    let path = path.to_string_lossy();
    let name = file_name(&path).trim_end_matches(['.', ' ']);
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    let extension = &name[dot..];
    extension.eq_ignore_ascii_case(".bat") || extension.eq_ignore_ascii_case(".cmd")
}

/// `arg` quoted for `CommandLineToArgvW`, exactly as std's `append_arg` does
/// without forced quotes: quotes only around empty arguments and ones with a
/// space or tab; backslashes doubled before a quote, and inner quotes escaped.
pub(crate) fn quote(arg: &str) -> String {
    let quoted = arg.is_empty() || arg.contains([' ', '\t']);
    let mut out = String::with_capacity(arg.len() + 2);
    if quoted {
        out.push('"');
    }
    let mut backslashes = 0;
    for character in arg.chars() {
        if character == '\\' {
            backslashes += 1;
        } else {
            if character == '"' {
                // n backslashes before a quote become 2n + 1.
                out.extend(std::iter::repeat_n('\\', backslashes + 1));
            }
            backslashes = 0;
        }
        out.push(character);
    }
    if quoted {
        // n backslashes before the closing quote become 2n.
        out.extend(std::iter::repeat_n('\\', backslashes));
        out.push('"');
    }
    out
}

/// The `cmd.exe` arguments that run batch file `script` with `args`, ported from
/// std's `make_bat_command_line` (the CVE-2024-24576 fix): `/e:ON` enables the
/// `%` escape below, `/v:OFF` turns off `!` expansion, `/d` skips AutoRun, and
/// the whole command sits in one more pair of quotes that `/c` strips.
fn batch_arguments(script: &str, args: &[String]) -> Result<String, CommandError> {
    let script = user_path(script);
    if script.contains('"') || script.ends_with('\\') {
        return Err(CommandError::InvalidProgram(script.into_owned()));
    }
    let mut out = format!("/e:ON /v:OFF /d /c \"\"{script}\"");
    for arg in args {
        // A line break ends the command cmd.exe reads, cutting the arguments short.
        if arg.contains(['\r', '\n']) {
            return Err(CommandError::ArgumentNotSafeForBatch(arg.clone()));
        }
        out.push(' ');
        out.push_str(&quote_batch(arg));
    }
    out.push('"');
    Ok(out)
}

/// `arg` quoted for a batch file the way std's `append_bat_arg` does: quoted
/// when empty, ending in `\` (so `"%~1"` in the script does not escape its own
/// closing quote), or holding any ASCII symbol outside a small safe set or a
/// control character; quotes doubled; `%` defused.
fn quote_batch(arg: &str) -> String {
    let quoted = arg.is_empty()
        || arg.ends_with('\\')
        || arg.chars().any(|character| {
            (character.is_ascii()
                && !(character.is_ascii_alphanumeric() || BATCH_UNQUOTED.contains(character)))
                || character.is_control()
        });
    let mut out = String::with_capacity(arg.len() + 2);
    if quoted {
        out.push('"');
    }
    let mut backslashes = 0;
    for character in arg.chars() {
        if character == '\\' {
            backslashes += 1;
        } else {
            if character == '"' {
                // n backslashes before a quote become 2n, and the quote doubles.
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push('"');
            } else if character == '%' {
                // `%%cd:~,%` expands to nothing (a zero-length substring of the
                // always-set `cd`), and it keeps cmd.exe from pairing this `%`
                // with another one into a variable reference.
                out.push_str("%%cd:~,");
            }
            backslashes = 0;
        }
        out.push(character);
    }
    if quoted {
        out.extend(std::iter::repeat_n('\\', backslashes));
        out.push('"');
    }
    out
}

/// `path` without a verbatim `\\?\` prefix, which cmd.exe cannot read.
fn user_path(path: &str) -> std::borrow::Cow<'_, str> {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}").into()
    } else if let Some(local) = path.strip_prefix(r"\\?\")
        && has_drive(local)
    {
        local.into()
    } else {
        path.into()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// The System32 directory the tests pretend to have.
    const SYSTEM: &str = r"C:\Windows\System32";

    /// An environment from `(name, value)` pairs.
    fn env(vars: &[(&str, &str)]) -> BTreeMap<String, String> {
        vars.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// Builds natively quoted with only `files` existing.
    fn build_in(
        program: &str,
        args: &[&str],
        env: &BTreeMap<String, String>,
        files: &[PathBuf],
    ) -> Result<Command, CommandError> {
        build_quoted(program, args, env, files, Quoting::Native)
    }

    /// Builds with `quoting` and only `files` existing.
    fn build_quoted(
        program: &str,
        args: &[&str],
        env: &BTreeMap<String, String>,
        files: &[PathBuf],
        quoting: Quoting,
    ) -> Result<Command, CommandError> {
        let files: BTreeSet<&PathBuf> = files.iter().collect();
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        build_with(
            program,
            &args,
            env,
            Path::new(r"C:\work"),
            Path::new(SYSTEM),
            quoting,
            &|path| files.contains(&path.to_path_buf()),
        )
    }

    /// `dir` joined with `name`, the way the resolver joins them.
    fn file(dir: &str, name: &str) -> PathBuf {
        Path::new(dir).join(name)
    }

    #[test]
    /// Arguments quote like std: only when needed, with backslashes doubled
    /// only in front of a quote.
    fn quoting_matches_std() {
        for (arg, expected) in [
            ("plain", "plain"),
            ("", r#""""#),
            ("a b", r#""a b""#),
            ("a\tb", "\"a\tb\""),
            (r#"a"b"#, r#"a\"b"#),
            (r#"""#, r#"\""#),
            (r"a\b", r"a\b"),
            (r"trailing\", r"trailing\"),
            (r"a b\", r#""a b\\""#),
            (r"a b\\", r#""a b\\\\""#),
            (r#"a\"b"#, r#"a\\\"b"#),
            (r#"say "hi" now"#, r#""say \"hi\" now""#),
            (r"\\server\share\", r"\\server\share\"),
            ("ünïcode", "ünïcode"),
        ] {
            assert_eq!(quote(arg), expected, "{arg:?}");
        }
    }

    #[test]
    /// MSYS quoting (`devlanes_shell_windows::quote_msys`) applies to argv0 and every
    /// argument of a shell unit.
    fn msys_quoting() {
        let sh = PathBuf::from(r"C:\Git\usr\bin\sh.exe");
        let command = build_quoted(
            &sh.to_string_lossy(),
            &["-c", r#"echo "a\b""#],
            &BTreeMap::new(),
            std::slice::from_ref(&sh),
            Quoting::Msys,
        )
        .expect("built");
        assert_eq!(command.application, sh);
        assert_eq!(
            command.command_line,
            r#""C:\\Git\\usr\\bin\\sh.exe" "-c" "echo \"a\\b\"""#
        );
    }

    #[test]
    /// argv0 is the program as given, in quotes, and arguments follow quoted.
    fn command_line_quotes_argv0_and_arguments() {
        let env = env(&[("PATH", r"C:\bin")]);
        let tool = file(r"C:\bin", "tool.exe");
        let command =
            build_in("tool", &["a b", "", "x"], &env, std::slice::from_ref(&tool)).expect("built");
        assert_eq!(command.application, tool);
        assert_eq!(command.command_line, r#""tool" "a b" "" x"#);
        let bare = build_in("tool", &[], &env, &[tool]).expect("built");
        assert_eq!(bare.command_line, r#""tool""#);
    }

    #[test]
    /// A bare name tries each PATH directory in order, and in each one the
    /// runnable PATHEXT extensions in PATHEXT order.
    fn pathext_resolution() {
        let env = env(&[
            ("PATH", r"C:\a;C:\b"),
            ("PATHEXT", ".COM;.EXE;.BAT;.CMD;.VBS;.JS"),
        ]);
        let found = |program: &str, files: &[PathBuf]| {
            resolve(program, &env, Path::new(r"C:\work"), &|path| {
                files.contains(&path.to_path_buf())
            })
            .ok()
        };
        // npm ships as npm.cmd; std alone would only look for npm.exe.
        let npm = file(r"C:\b", "npm.cmd");
        assert_eq!(found("npm", std::slice::from_ref(&npm)), Some(npm.clone()));
        // An earlier directory wins over a preferred extension later on.
        let early = file(r"C:\a", "npm.cmd");
        let late = file(r"C:\b", "npm.exe");
        assert_eq!(found("npm", &[early.clone(), late]), Some(early));
        // Within one directory, PATHEXT order decides.
        let exe = file(r"C:\a", "tool.exe");
        let cmd = file(r"C:\a", "tool.cmd");
        assert_eq!(found("tool", &[cmd, exe.clone()]), Some(exe));
        // A name with an extension is tried as given first.
        assert_eq!(found("npm.cmd", std::slice::from_ref(&npm)), Some(npm));
        let versioned = file(r"C:\a", "python3.12.exe");
        assert_eq!(
            found("python3.12", std::slice::from_ref(&versioned)),
            Some(versioned)
        );
        // Without an extension the bare file is not a candidate, and
        // non-runnable PATHEXT entries are skipped.
        assert_eq!(found("tool", &[file(r"C:\a", "tool")]), None);
        assert_eq!(found("tool", &[file(r"C:\a", "tool.js")]), None);
    }

    #[test]
    /// PATHEXT order is honoured, and an unset or useless PATHEXT falls back
    /// to `.COM;.EXE;.BAT;.CMD`.
    fn pathext_order_and_default() {
        let exe = file(r"C:\a", "tool.exe");
        let cmd = file(r"C:\a", "tool.cmd");
        let files = [exe.clone(), cmd.clone()];
        let found = |vars: &[(&str, &str)]| {
            resolve("tool", &env(vars), Path::new(r"C:\work"), &|path| {
                files.contains(&path.to_path_buf())
            })
            .ok()
        };
        assert_eq!(
            found(&[("PATH", r"C:\a"), ("PATHEXT", ".cmd;.exe")]),
            Some(cmd)
        );
        assert_eq!(found(&[("PATH", r"C:\a")]), Some(exe.clone()));
        assert_eq!(found(&[("PATH", r"C:\a"), ("PATHEXT", ".JS")]), Some(exe));
    }

    #[test]
    /// `Path` and `PathExt` count as `PATH` and `PATHEXT`; the exact spelling
    /// wins when both exist.
    fn variable_names_ignore_case() {
        let env = env(&[("Path", r"C:\a"), ("PathExt", ".CMD")]);
        let command = build_in("npm", &[], &env, &[file(r"C:\a", "npm.cmd")]).expect("built");
        assert_eq!(command.application, file(SYSTEM, "cmd.exe"));
        let both = self::env(&[("PATH", r"C:\a"), ("Path", r"C:\b")]);
        assert_eq!(var(&both, "PATH"), Some(r"C:\a"));
    }

    #[test]
    /// A miss lists the directories searched: empty entries skipped, quotes
    /// dropped, relative entries taken from the working directory.
    fn not_found_lists_searched_directories() {
        let env = env(&[("PATH", r#"C:\a;;"C:\semi;colon";tools;"#)]);
        let error = build_in("missing", &[], &env, &[]).expect_err("missing");
        let CommandError::NotFound { program, searched } = error else {
            panic!("not found expected, got {error:?}");
        };
        assert_eq!(program, PathBuf::from("missing"));
        assert_eq!(
            searched,
            [
                PathBuf::from(r"C:\a"),
                PathBuf::from(r"C:\semi;colon"),
                Path::new(r"C:\work").join("tools"),
            ]
        );
        let empty = build_in("missing", &[], &BTreeMap::new(), &[]).expect_err("no PATH");
        assert!(matches!(empty, CommandError::NotFound { searched, .. } if searched.is_empty()));
    }

    #[test]
    /// A program with a separator or drive is a path: relative ones come from
    /// the working directory, PATH is not searched, PATHEXT still applies.
    fn explicit_paths() {
        let env = env(&[("PATH", r"C:\bin")]);
        let relative = Path::new(r"C:\work").join(r"scripts\build");
        let mut build_cmd = relative.as_os_str().to_owned();
        build_cmd.push(".cmd");
        let resolved = build_in(r"scripts\build", &[], &env, &[PathBuf::from(&build_cmd)]);
        assert!(resolved.is_ok(), "{resolved:?}");
        let absolute = PathBuf::from(r"C:\tools\x.exe");
        let command = build_in(
            r"C:\tools\x.exe",
            &[],
            &env,
            std::slice::from_ref(&absolute),
        )
        .expect("built");
        assert_eq!(command.application, absolute);
        assert_eq!(command.command_line, r#""C:\tools\x.exe""#);
        // On PATH only, so an explicit path misses it.
        let error = build_in(r".\tool", &[], &env, &[file(r"C:\bin", "tool.exe")])
            .expect_err("not on the path given");
        let CommandError::NotFound { searched, .. } = error else {
            panic!("not found expected, got {error:?}");
        };
        let named = Path::new(r"C:\work").join(r".\tool");
        assert_eq!(
            searched,
            named
                .parent()
                .map(Path::to_path_buf)
                .into_iter()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    /// Bad programs and arguments are refused before anything is resolved.
    fn invalid_input() {
        let env = env(&[("PATH", r"C:\bin")]);
        assert!(matches!(
            build_in("", &[], &env, &[]),
            Err(CommandError::InvalidProgram(_))
        ));
        assert!(matches!(
            build_in(r"tools\", &[], &env, &[]),
            Err(CommandError::InvalidProgram(_))
        ));
        assert!(matches!(
            build_in("tool", &["a\0b"], &env, &[file(r"C:\bin", "tool.exe")]),
            Err(CommandError::ContainsNul(_))
        ));
        // Relative to drive C's current directory, which is the daemon's.
        assert!(matches!(
            build_in("C:tool", &[], &env, &[]),
            Err(CommandError::InvalidProgram(_))
        ));
    }

    #[test]
    /// A name is tried as given only with an extension Windows can start, and
    /// drive-relative PATH entries are skipped.
    fn only_runnable_names_and_usable_path_entries() {
        let env = env(&[("PATH", r"C:dir;C:\bin")]);
        // `npm` (an sh script) sits next to `npm.cmd`; `npm.` must not pick it.
        let files = [file(r"C:\bin", "npm."), file(r"C:\bin", "npm.cmd")];
        let error = build_in("npm.", &[], &env, &files).expect_err("npm. is not runnable");
        let CommandError::NotFound { searched, .. } = error else {
            panic!("not found expected, got {error:?}");
        };
        assert_eq!(searched, [PathBuf::from(r"C:\bin")]);
        assert!(build_in("tool.js", &[], &env, &[file(r"C:\bin", "tool.js")]).is_err());
        let exe = build_in("tool.exe", &[], &env, &[file(r"C:\bin", "tool.exe")]);
        assert!(exe.is_ok(), "{exe:?}");
    }

    #[test]
    /// Batch files run through `<System32>\cmd.exe` with std's flags and
    /// escaping.
    fn batch_files_run_through_cmd() {
        let env = env(&[("PATH", r"C:\b")]);
        let script = file(r"C:\b", "npm.cmd");
        let command = build_in(
            "npm",
            &["run", "dev server"],
            &env,
            std::slice::from_ref(&script),
        )
        .expect("built");
        assert_eq!(command.application, file(SYSTEM, "cmd.exe"));
        let arguments = format!(
            r#"/e:ON /v:OFF /d /c ""{}" run "dev server"""#,
            script.display()
        );
        assert_eq!(
            command.command_line,
            OsString::from(format!("cmd.exe {arguments}"))
        );
        // Trailing dots and spaces are dropped from Windows file names, so
        // this is still a batch file and must not skip the escaping.
        let sneaky = PathBuf::from(r"C:\b\run.bat.");
        let command = build_in(r"C:\b\run.bat.", &["&calc"], &env, &[sneaky]).expect("built");
        assert_eq!(command.application, file(SYSTEM, "cmd.exe"));
        assert!(
            command
                .command_line
                .to_string_lossy()
                .ends_with(r#" "&calc"""#)
        );
        // Batch escaping applies whatever quoting the caller asked for.
        let msys = build_quoted(
            "npm",
            &["a b"],
            &env,
            std::slice::from_ref(&script),
            Quoting::Msys,
        )
        .expect("built");
        assert!(msys.command_line.to_string_lossy().ends_with(r#" "a b"""#));
        assert!(is_batch_file(Path::new(r"C:\b\run.bat. .")));
        assert!(is_batch_file(Path::new(r"C:\b\RUN.CMD")));
        assert!(!is_batch_file(Path::new(r"C:\b\run.exe")));
        assert!(!is_batch_file(Path::new(r"C:\b.cmd\run")));
    }

    #[test]
    /// Batch escaping quotes everything cmd.exe could read as syntax and defuses
    /// `%`; line breaks are refused.
    fn batch_escaping() {
        for (arg, expected) in [
            ("plain", "plain"),
            ("safe-arg_1.txt", "safe-arg_1.txt"),
            (r"C:\dir\file", r"C:\dir\file"),
            ("", r#""""#),
            ("a b", r#""a b""#),
            (r"C:\dir\", r#""C:\dir\\""#),
            (r#"a"b"#, r#""a""b""#),
            (r#"a\"b"#, r#""a\\""b""#),
            ("&calc", r#""&calc""#),
            ("a|b<c>d^e", r#""a|b<c>d^e""#),
            ("%PATH%", r#""%%cd:~,%PATH%%cd:~,%""#),
            ("!x!", r#""!x!""#),
            ("tab\there", "\"tab\there\""),
            ("ünï", "ünï"),
        ] {
            assert_eq!(quote_batch(arg), expected, "{arg:?}");
        }
        for arg in ["a\nb", "a\rb", "line\r\n"] {
            let error = batch_arguments(r"C:\b\x.bat", &[arg.to_owned()]).expect_err("refused");
            assert!(
                matches!(&error, CommandError::ArgumentNotSafeForBatch(refused) if refused == arg),
                "{error:?}"
            );
        }
        assert!(matches!(
            batch_arguments(r#"C:\b\"x.bat"#, &[]),
            Err(CommandError::InvalidProgram(_))
        ));
        assert_eq!(
            batch_arguments(r"\\?\C:\b\x.bat", &[]).expect("built"),
            r#"/e:ON /v:OFF /d /c ""C:\b\x.bat"""#
        );
        assert_eq!(
            batch_arguments(r"\\?\UNC\host\share\x.bat", &[]).expect("built"),
            r#"/e:ON /v:OFF /d /c ""\\host\share\x.bat"""#
        );
    }

    #[test]
    /// Windows absolute paths are recognised.
    fn absolute_paths() {
        for path in [r"C:\x", "c:/x", r"\\server\share", "//server/share"] {
            assert!(is_absolute(path), "{path}");
        }
        for path in [r"\x", "C:x", "x", r".\x"] {
            assert!(!is_absolute(path), "{path}");
        }
    }
}
