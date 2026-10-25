//! The PowerShell reader. Besides a trailing `&`, PowerShell detaches a
//! command with `Start-Process` (without `-Wait`), `Start-Job`,
//! `Start-ThreadJob` and cmd's `start`.

use std::path::{Path, PathBuf};

use crate::{moved, program};

/// Whether `script` starts a dev server in the background inside a project.
/// `detached` holds for all of it, as in a job's script block or a call with
/// `run_in_background`. A `cd` moves the directory for the commands after it.
pub(crate) fn blocks(script: &str, detached: bool, cwd: &Path, home: Option<&Path>) -> bool {
    script_blocks(script, detached, &mut cwd.to_path_buf(), home)
}

/// A piece of a PowerShell script.
#[derive(Debug)]
enum Token {
    /// A word with its quotes and escapes removed. `dash` when it starts with
    /// a bare `-`, so it may name a parameter: `-Wait` does, `'-Wait'` does
    /// not.
    Word { text: String, dash: bool },
    /// The text inside `{ }`: a script block, hashtable or `${name}`.
    Block(String),
    /// A `,` between the elements of an array argument: `'run','dev'`.
    Comma,
    /// `|`, `&&` or `||`: the next command is part of the same chain.
    Join,
    /// `;` or a line break.
    End,
    /// A `&` that ends a chain, which PowerShell 7 then runs as a job.
    Background,
}

/// One argument word of a command.
#[derive(Debug, Clone, Copy)]
struct Arg<'a> {
    text: &'a str,
    /// Starts with a bare `-`.
    dash: bool,
    /// Follows a `,`: another element of the array before it.
    joined: bool,
}

/// Whether `script` starts a dev server in the background inside a project,
/// with `dir` moved by its `cd`s.
fn script_blocks(script: &str, detached: bool, dir: &mut PathBuf, home: Option<&Path>) -> bool {
    commands(lex(script))
        .into_iter()
        .any(|(tokens, background)| command_blocks(&tokens, detached || background, dir, home))
}

/// The lexer's state for the word being read.
#[derive(Default)]
struct WordState {
    text: String,
    /// Set once a word has started, so `''` is an empty word.
    started: bool,
    dash: bool,
}

impl WordState {
    /// Ends the current word, if one started.
    fn end(&mut self, tokens: &mut Vec<Token>) {
        if self.started {
            tokens.push(Token::Word {
                text: std::mem::take(&mut self.text),
                dash: self.dash,
            });
        }
        self.started = false;
        self.dash = false;
    }
}

/// Splits `script` into tokens. Comments (`#`, `<# #>`) are dropped, and so is
/// the call operator `&` at the start of a command (`& 'C:\tools\vite.cmd'`).
fn lex(script: &str) -> Vec<Token> {
    let chars: Vec<char> = script.chars().collect();
    let mut tokens = Vec::new();
    let mut word = WordState::default();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\'' => {
                word.started = true;
                i = single_quoted(&chars, i + 1, &mut word.text);
                continue;
            }
            '"' => {
                word.started = true;
                i = double_quoted(&chars, i + 1, &mut word.text);
                continue;
            }
            // A backtick before a line break continues the line.
            '`' if matches!(next, Some('\n' | '\r')) => {
                i += if chars.get(i + 2) == Some(&'\n') {
                    3
                } else {
                    2
                };
                continue;
            }
            '`' => {
                word.started = true;
                word.text.extend(next);
                i += 2;
                continue;
            }
            '@' if !word.started && matches!(next, Some('\'' | '"')) => {
                word.started = true;
                match here_string(&chars, i) {
                    Some((body, end)) => {
                        word.text.push_str(&body);
                        i = end;
                        continue;
                    }
                    None => word.text.push(c),
                }
            }
            '#' if !word.started => {
                while chars.get(i).is_some_and(|&c| c != '\n') {
                    i += 1;
                }
                continue;
            }
            '<' if !word.started && next == Some('#') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '#' && chars.get(i + 1) == Some(&'>')) {
                    i += 1;
                }
                i += 2;
                continue;
            }
            '{' => {
                word.end(&mut tokens);
                let end = block_end(&chars, i + 1);
                tokens.push(Token::Block(chars[i + 1..end].iter().collect()));
                i = end + 1;
                continue;
            }
            ' ' | '\t' | '\r' | '(' | ')' | '}' => word.end(&mut tokens),
            ',' => {
                word.end(&mut tokens);
                tokens.push(Token::Comma);
            }
            '@' if next == Some('(') => word.end(&mut tokens),
            ';' | '\n' => {
                word.end(&mut tokens);
                tokens.push(Token::End);
            }
            '|' => {
                word.end(&mut tokens);
                if next == Some('|') {
                    i += 1;
                }
                tokens.push(Token::Join);
            }
            '&' if next == Some('&') => {
                word.end(&mut tokens);
                i += 1;
                tokens.push(Token::Join);
            }
            // A redirection such as `2>&1`.
            '&' if word.started && word.text.ends_with('>') => word.text.push(c),
            // The call operator, also in `$p = & npm run dev`.
            '&' if !word.started
                && match tokens.last() {
                    None | Some(Token::End | Token::Join) => true,
                    Some(Token::Word { text, .. }) => text == "=",
                    _ => false,
                } => {}
            '&' => {
                word.end(&mut tokens);
                tokens.push(Token::Background);
            }
            _ => {
                if !word.started {
                    word.started = true;
                    word.dash = c == '-';
                }
                word.text.push(c);
            }
        }
        i += 1;
    }
    word.end(&mut tokens);
    tokens
}

/// Reads a `'...'` string from just after its quote into `word`, where `''`
/// is one quote, and returns the index after the closing quote.
fn single_quoted(chars: &[char], mut i: usize, word: &mut String) -> usize {
    while i < chars.len() {
        if chars[i] == '\'' {
            if chars.get(i + 1) != Some(&'\'') {
                return i + 1;
            }
            i += 1;
        }
        word.push(chars[i]);
        i += 1;
    }
    i
}

/// Reads a `"..."` string from just after its quote into `word`, where
/// `` ` `` escapes and `""` is one quote, and returns the index after the
/// closing quote.
fn double_quoted(chars: &[char], mut i: usize, word: &mut String) -> usize {
    while i < chars.len() {
        match chars[i] {
            '`' if i + 1 < chars.len() => {
                word.push(chars[i + 1]);
                i += 2;
            }
            '"' if chars.get(i + 1) == Some(&'"') => {
                word.push('"');
                i += 2;
            }
            '"' => return i + 1,
            c => {
                word.push(c);
                i += 1;
            }
        }
    }
    i
}

/// The body of a here-string whose `@` is at `at`, and the index after it.
/// `@'` or `@"` ends its line, and the string closes at a line that starts
/// with `'@` or `"@`; unclosed, it runs to the end. `None` when `@'` is not a
/// here-string. Read as one word, so a `'` or `&` in a commit message neither
/// quotes nor splits.
fn here_string(chars: &[char], at: usize) -> Option<(String, usize)> {
    let quote = chars[at + 1];
    let mut start = at + 2;
    while matches!(chars.get(start), Some(' ' | '\t' | '\r')) {
        start += 1;
    }
    if chars.get(start) != Some(&'\n') {
        return None;
    }
    start += 1;
    let mut i = start;
    while i < chars.len() {
        if (i == start || chars[i - 1] == '\n')
            && chars[i] == quote
            && chars.get(i + 1) == Some(&'@')
        {
            return Some((chars[start..i].iter().collect(), i + 2));
        }
        i += 1;
    }
    Some((chars[start..].iter().collect(), chars.len()))
}

/// The index of the `}` that closes a block whose text starts at `i`, or the
/// end of the script. Braces inside quotes do not count.
fn block_end(chars: &[char], mut i: usize) -> usize {
    let mut depth = 0;
    let mut quoted = String::new();
    while i < chars.len() {
        match chars[i] {
            '\'' => {
                i = single_quoted(chars, i + 1, &mut quoted);
                continue;
            }
            '"' => {
                i = double_quoted(chars, i + 1, &mut quoted);
                continue;
            }
            '`' => i += 1,
            '{' => depth += 1,
            '}' if depth == 0 => return i,
            '}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

/// Groups tokens into simple commands, each with whether it runs in the
/// background. Commands chained with `|`, `&&` or `||` share the `&` that ends
/// their chain, as in Bash.
fn commands(tokens: Vec<Token>) -> Vec<(Vec<Token>, bool)> {
    let mut commands = Vec::new();
    let mut chain = Vec::new();
    let mut current = Vec::new();
    for token in tokens {
        let background = match token {
            Token::Join => {
                chain.push(std::mem::take(&mut current));
                continue;
            }
            Token::End => false,
            Token::Background => true,
            word => {
                current.push(word);
                continue;
            }
        };
        chain.push(std::mem::take(&mut current));
        commands.extend(chain.drain(..).map(|command| (command, background)));
    }
    chain.push(current);
    commands.extend(chain.drain(..).map(|command| (command, false)));
    commands.retain(|(command, _)| !command.is_empty());
    commands
}

/// Whether one simple command starts a dev server in the background inside a
/// project. Script blocks of other commands (`if`, `ForEach-Object`) are
/// judged like the script around them.
fn command_blocks(
    tokens: &[Token],
    detached: bool,
    dir: &mut PathBuf,
    home: Option<&Path>,
) -> bool {
    let (mut args, mut blocks) = (Vec::new(), Vec::new());
    let mut joined = false;
    for token in tokens {
        match token {
            Token::Word { text, dash } => args.push(Arg {
                text,
                dash: *dash,
                joined: std::mem::take(&mut joined),
            }),
            Token::Comma => joined = true,
            Token::Block(block) => blocks.push(block.as_str()),
            _ => {}
        }
    }
    // `$job = Start-Job { ... }`, `$p=Start-Process ...`
    if let Some(&first) = args.first().filter(|arg| arg.text.starts_with('$')) {
        if args.get(1).is_some_and(|arg| arg.text == "=") {
            args.drain(..2);
        } else if let Some((_, value)) = first.text.split_once('=') {
            if value.is_empty() {
                args.remove(0);
            } else {
                args[0].text = value;
            }
        }
    }
    let name = args
        .first()
        .map(|first| program(first.text))
        .unwrap_or_default();
    let rest = args.get(1..).unwrap_or_default();
    let words: Vec<&str> = args.iter().map(|arg| arg.text).collect();
    match name.as_str() {
        "cd" | "chdir" | "sl" | "set-location" | "pushd" | "push-location" => {
            change_dir(rest, dir, home);
            false
        }
        "start-process" | "saps" | "start" => start_process(rest, detached, dir, home),
        "start-job" | "sajb" | "start-threadjob" => {
            let dir = job_directory(rest, dir, home);
            blocks
                .iter()
                .any(|block| script_blocks(block, true, &mut dir.clone(), home))
        }
        "cmd" => cmd(&words[1..], detached, dir),
        _ => {
            (detached && serves(&words, dir))
                || blocks
                    .iter()
                    .any(|block| script_blocks(block, detached, &mut dir.clone(), home))
        }
    }
}

/// `Start-Process` returns at once unless `-Wait` (`-Wait:$false` does not
/// wait), and starts nothing with `-WhatIf`. With `-Wait` it still counts
/// when `detached` holds, since the whole call runs in the background. The
/// program is `-FilePath`, else the first positional argument;
/// `-ArgumentList` and the other positional
/// arguments are its arguments, and it runs in `-WorkingDirectory`, else
/// `dir`. Parameters come in any order, with PowerShell's aliases and
/// unambiguous prefixes. A word that names no `Start-Process` parameter, such
/// as `--watch`, is an argument. Opening a URL starts a browser, not a server.
fn start_process(args: &[Arg], detached: bool, dir: &Path, home: Option<&Path>) -> bool {
    let mut groups = array_groups(args).into_iter();
    let (mut file_path, mut working_directory) = (None, None);
    let mut arguments = Vec::new();
    let mut positional = Vec::new();
    let mut parameters_end = false;
    while let Some(group) = groups.next() {
        let first = group[0];
        if first.dash && !parameters_end {
            if first.text == "--" {
                parameters_end = true;
                continue;
            }
            let (name, inline) = match first.text[1..].split_once(':') {
                Some((name, value)) => (name, Some(value)),
                None => (&first.text[1..], None),
            };
            if let Some(parameter) = start_process_parameter(name) {
                let value: Vec<&str> = match inline {
                    Some(inline) => std::iter::once(inline)
                        .chain(group[1..].iter().map(|arg| arg.text))
                        .collect(),
                    None if parameter.switch => Vec::new(),
                    None => groups
                        .next()
                        .map(|value| value.iter().map(|arg| arg.text).collect())
                        .unwrap_or_default(),
                };
                match parameter.name {
                    "wait" | "whatif" => {
                        let on = !value
                            .first()
                            .is_some_and(|value| matches!(*value, "$false" | "0"));
                        if on && (parameter.name == "whatif" || !detached) {
                            return false;
                        }
                    }
                    "filepath" => file_path = value.first().copied(),
                    "argumentlist" => arguments.extend(value),
                    "workingdirectory" => working_directory = value.first().copied(),
                    _ => {}
                }
                continue;
            }
        }
        positional.extend(group.iter().map(|arg| arg.text));
    }
    let mut positional = positional.into_iter();
    let Some(program) = file_path.or_else(|| positional.next()) else {
        return false;
    };
    let target: Vec<&str> = std::iter::once(program)
        .chain(positional)
        .chain(arguments)
        .collect();
    let dir = working_directory.map_or_else(|| dir.to_path_buf(), |value| moved(dir, value, home));
    start_target(&target, &dir)
}

/// The arguments grouped into PowerShell arguments: `'run','dev'` is one
/// array argument.
fn array_groups<'a>(args: &[Arg<'a>]) -> Vec<Vec<Arg<'a>>> {
    let mut groups: Vec<Vec<Arg>> = Vec::new();
    for &arg in args {
        match groups.last_mut() {
            Some(group) if arg.joined => group.push(arg),
            _ => groups.push(vec![arg]),
        }
    }
    groups
}

/// A `Start-Process` parameter, in lowercase.
struct Parameter {
    name: &'static str,
    aliases: &'static [&'static str],
    /// Takes no value unless written `-Name:value`.
    switch: bool,
    /// A common parameter (or `-WhatIf`, `-Confirm`), which loses to a
    /// `Start-Process` parameter when a prefix matches both.
    common: bool,
}

/// `Start-Process`'s parameters in Windows PowerShell 5.1 and PowerShell 7.
const START_PROCESS_PARAMETERS: &[Parameter] = &[
    parameter("filepath", &["pspath", "path"], false, false),
    parameter("argumentlist", &["args"], false, false),
    parameter("credential", &["runas"], false, false),
    parameter("workingdirectory", &[], false, false),
    parameter("loaduserprofile", &["lup"], true, false),
    parameter("nonewwindow", &["nnw"], true, false),
    parameter("passthru", &[], true, false),
    parameter("redirectstandarderror", &["rse"], false, false),
    parameter("redirectstandardinput", &["rsi"], false, false),
    parameter("redirectstandardoutput", &["rso"], false, false),
    parameter("verb", &[], false, false),
    parameter("windowstyle", &[], false, false),
    parameter("wait", &[], true, false),
    parameter("usenewenvironment", &[], true, false),
    parameter("environment", &[], false, false),
    parameter("whatif", &["wi"], true, true),
    parameter("confirm", &["cf"], true, true),
    parameter("verbose", &["vb"], true, true),
    parameter("debug", &["db"], true, true),
    parameter("erroraction", &["ea"], false, true),
    parameter("warningaction", &["wa"], false, true),
    parameter("informationaction", &["infa"], false, true),
    parameter("progressaction", &["proga"], false, true),
    parameter("errorvariable", &["ev"], false, true),
    parameter("warningvariable", &["wv"], false, true),
    parameter("informationvariable", &["iv"], false, true),
    parameter("outvariable", &["ov"], false, true),
    parameter("outbuffer", &["ob"], false, true),
    parameter("pipelinevariable", &["pv"], false, true),
];

/// Builds one entry of [`START_PROCESS_PARAMETERS`].
const fn parameter(
    name: &'static str,
    aliases: &'static [&'static str],
    switch: bool,
    common: bool,
) -> Parameter {
    Parameter {
        name,
        aliases,
        switch,
        common,
    }
}

/// The `Start-Process` parameter `-name` binds to, the way PowerShell
/// resolves it: a whole name or alias, else the one parameter a prefix of
/// its names or aliases matches, preferring a `Start-Process` parameter over
/// common ones. `None` for an unknown or ambiguous name.
fn start_process_parameter(name: &str) -> Option<&'static Parameter> {
    let name = name.to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let names = |parameter: &Parameter| {
        std::iter::once(parameter.name)
            .chain(parameter.aliases.iter().copied())
            .collect::<Vec<_>>()
    };
    if let Some(exact) = START_PROCESS_PARAMETERS
        .iter()
        .find(|parameter| names(parameter).contains(&name.as_str()))
    {
        return Some(exact);
    }
    let matches: Vec<&Parameter> = START_PROCESS_PARAMETERS
        .iter()
        .filter(|parameter| {
            names(parameter)
                .iter()
                .any(|candidate| candidate.starts_with(&name))
        })
        .collect();
    if let [only] = matches.as_slice() {
        return Some(only);
    }
    match matches
        .iter()
        .filter(|parameter| !parameter.common)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [only] => Some(only),
        _ => None,
    }
}

/// The command a `Start-Process` or cmd `start` starts, judged as detached.
fn start_target(target: &[&str], dir: &Path) -> bool {
    !target.first().is_some_and(|first| first.contains("://")) && serves(target, dir)
}

/// `-WorkingDirectory DIR` or `-WorkingDirectory:DIR` of a `Start-Job`,
/// resolved against `dir`, else `dir`.
fn job_directory(args: &[Arg], dir: &Path, home: Option<&Path>) -> PathBuf {
    args.iter()
        .enumerate()
        .filter(|(_, arg)| arg.dash)
        .find_map(|(at, arg)| {
            let name = arg.text[1..].to_ascii_lowercase();
            if name == "workingdirectory" {
                return args.get(at + 1).map(|value| value.text);
            }
            name.strip_prefix("workingdirectory:")?;
            arg.text.split_once(':').map(|(_, value)| value)
        })
        .map_or_else(|| dir.to_path_buf(), |value| moved(dir, value, home))
}

/// `cmd /c COMMAND` (or `/k`). cmd's `start` returns at once unless `/wait`
/// (which still counts when `detached` holds), and runs in `/d DIR`, else
/// `dir`; any other command runs in the foreground of the PowerShell command
/// around it.
fn cmd(args: &[&str], detached: bool, dir: &Path) -> bool {
    let Some(at) = args
        .iter()
        .position(|arg| arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("/k"))
    else {
        return false;
    };
    let tokens = cmd_tokens(&args[at + 1..]);
    let words: Vec<&str> = tokens.iter().map(|(word, _)| word.as_str()).collect();
    let Some((first, rest)) = words.split_first() else {
        return false;
    };
    if !first.eq_ignore_ascii_case("start") {
        return detached && serves(&words, dir);
    }
    let mut dir = dir.to_path_buf();
    let mut target = Vec::new();
    let mut rest = rest.iter().zip(&tokens[1..]).peekable();
    // A quoted first word is the window title: `start "Dev Server" /wait pnpm dev`.
    rest.next_if(|(_, (_, quoted))| *quoted);
    while let Some((&arg, _)) = rest.next() {
        if !target.is_empty() || !arg.starts_with('/') {
            target.push(arg);
        } else if arg.eq_ignore_ascii_case("/wait") {
            if !detached {
                return false;
            }
        } else if arg.eq_ignore_ascii_case("/d")
            && let Some((value, _)) = rest.next()
        {
            dir = moved(&dir, value, None);
        }
    }
    start_target(&target, &dir)
}

/// The words of the line cmd runs for `cmd /c ARGS`, each with whether it
/// was in double quotes. PowerShell drops empty arguments and passes one with
/// a blank in double quotes; cmd then removes the first and last quote of a
/// line that starts with one, so `cmd /c "start pnpm dev"` reads as
/// unquoted, and splits the rest on blanks outside quotes.
fn cmd_tokens(args: &[&str]) -> Vec<(String, bool)> {
    let mut line = args
        .iter()
        .filter(|arg| !arg.is_empty())
        .map(|arg| {
            if arg.contains(char::is_whitespace) {
                format!("\"{arg}\"")
            } else {
                (*arg).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if line.starts_with('"') {
        line.remove(0);
        if let Some(last) = line.rfind('"') {
            line.remove(last);
        }
    }
    let mut tokens = Vec::new();
    let (mut word, mut quoted, mut in_quotes, mut in_word) = (String::new(), false, false, false);
    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                quoted = true;
                in_word = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if in_word {
                    tokens.push((std::mem::take(&mut word), quoted));
                }
                (quoted, in_word) = (false, false);
            }
            c => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        tokens.push((word, quoted));
    }
    tokens
}

/// `Set-Location DIR` and its aliases, also with `-Path` or `-LiteralPath`.
/// `cd -` goes back to a directory this hook does not know, so it stays put.
fn change_dir(args: &[Arg], dir: &mut PathBuf, home: Option<&Path>) {
    if let Some(target) = args.iter().find(|arg| !arg.dash) {
        *dir = moved(dir, target.text, home);
    }
}

/// Whether `words`, started in the background in `dir`, is a dev server
/// inside a project. Quoted arguments split on blanks, so `-ArgumentList 'run
/// dev'` reads `dev`; the program stays one word, since its path may hold a
/// space (`& "C:\Program Files\nodejs\npm.cmd" run dev &`).
fn serves(words: &[&str], dir: &Path) -> bool {
    let Some((&first, rest)) = words.split_first() else {
        return false;
    };
    let mut split = vec![first];
    split.extend(rest.iter().flat_map(|word| word.split_whitespace()));
    crate::serves(&split, dir)
}
