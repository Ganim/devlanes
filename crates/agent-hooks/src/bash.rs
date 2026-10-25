//! The Bash reader. It splits a command line into simple commands at `;`,
//! line breaks and a lone `&`, keeping quotes in the words, and judges each
//! one after its `cd`s.

use std::path::{Path, PathBuf};

/// Whether `command` starts a dev server in the background inside a project.
/// A command is detached when it ends with a lone `&` (shared by the commands
/// joined to it with `&&`, `||` or `|`), runs under `setsid -f`, `tmux new
/// -d` or `screen -dm`, or `background` holds.
pub(crate) fn blocks(command: &str, background: bool, cwd: &Path, home: Option<&Path>) -> bool {
    let mut dir = cwd.to_path_buf();
    for (backgrounded, segment) in segments(command) {
        let all: Vec<&str> = segment.split_whitespace().collect();
        let mut words = all.as_slice();
        while let Some((&first, rest)) = words.split_first() {
            if !assignment(first) && !matches!(first, "nohup" | "exec" | "command" | "time" | "env")
            {
                break;
            }
            words = rest;
        }
        let Some((&first, rest)) = words.split_first() else {
            continue;
        };
        if first == "cd" {
            change_dir(rest.first().copied(), &mut dir, home);
            continue;
        }
        let mut detached = backgrounded || background;
        match first {
            "setsid" => {
                words = rest;
                while let Some((&option, rest)) = words.split_first() {
                    if option == "--fork"
                        || option.strip_prefix('-').is_some_and(|o| o.contains('f'))
                    {
                        detached = true;
                    } else if !option.starts_with('-') {
                        break;
                    }
                    words = rest;
                }
            }
            "tmux"
                if rest
                    .iter()
                    .any(|word| matches!(*word, "new" | "new-session")) =>
            {
                detached |= rest.iter().any(|word| {
                    !word.starts_with("--") && word.starts_with('-') && word.contains('d')
                });
            }
            "screen" => {
                let flags: String = rest
                    .iter()
                    .filter(|word| word.starts_with('-'))
                    .copied()
                    .collect();
                detached |= flags.contains('d') && flags.contains('m');
            }
            _ => {}
        }
        let words: Vec<&str> = words.iter().map(|word| unquoted(word)).collect();
        if detached && crate::serves(&words, &dir) {
            return true;
        }
    }
    false
}

/// The simple commands of `command`, each with whether a lone `&` ends its
/// list. Commands joined by `&&`, `||` or `|` share the `&` that ends their
/// list. Operators inside quotes and redirections (`2>&1`, `&>`) do not split.
fn segments(command: &str) -> Vec<(bool, String)> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut list = Vec::new();
    let mut segment = String::new();
    let mut quote = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if quote == Some('\'') {
            segment.push(c);
            if c == '\'' {
                quote = None;
            }
            continue;
        }
        if c == '\\' {
            segment.push(c);
            segment.extend(chars.get(i));
            i += 1;
            continue;
        }
        if quote == Some('"') {
            segment.push(if c == '\n' { ' ' } else { c });
            if c == '"' {
                quote = None;
            }
            continue;
        }
        let next = chars.get(i).copied();
        let previous = (i >= 2).then(|| chars[i - 2]);
        match c {
            '\'' | '"' => {
                quote = Some(c);
                segment.push(c);
            }
            '&' if next == Some('&') => {
                i += 1;
                end_command(&mut segment, &mut list);
            }
            '&' if next == Some('>') || matches!(previous, Some('>' | '<')) => segment.push(c),
            '&' => end_list(true, &mut segment, &mut list, &mut segments),
            '|' => {
                if matches!(next, Some('|' | '&')) {
                    i += 1;
                }
                end_command(&mut segment, &mut list);
            }
            ';' | '\n' => end_list(false, &mut segment, &mut list, &mut segments),
            c => segment.push(c),
        }
    }
    end_list(false, &mut segment, &mut list, &mut segments);
    segments
}

/// Adds the command in `segment` to `list`, without the blanks, subshell
/// parentheses and group braces around it.
fn end_command(segment: &mut String, list: &mut Vec<String>) {
    let command = segment
        .trim_start_matches([' ', '\t', '(', '{'])
        .trim_end_matches([' ', '\t', ')', '}']);
    if !command.is_empty() {
        list.push(command.to_owned());
    }
    segment.clear();
}

/// Ends the list of commands joined so far, run in the background or not.
fn end_list(
    background: bool,
    segment: &mut String,
    list: &mut Vec<String>,
    segments: &mut Vec<(bool, String)>,
) {
    end_command(segment, list);
    segments.extend(list.drain(..).map(|command| (background, command)));
}

/// `NAME=value` before a command.
fn assignment(word: &str) -> bool {
    word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && word.contains('=')
}

/// `word` without one quote at its start and one at its end.
fn unquoted(word: &str) -> &str {
    let word = word.strip_prefix(['"', '\'']).unwrap_or(word);
    word.strip_suffix(['"', '\'']).unwrap_or(word)
}

/// `cd TARGET`: no target goes home, and `cd -` goes back to a directory this
/// hook does not know, so it stays put.
fn change_dir(target: Option<&str>, dir: &mut PathBuf, home: Option<&Path>) {
    match target.map(unquoted) {
        None => {
            if let Some(home) = home {
                *dir = home.to_path_buf();
            }
        }
        Some("-") => {}
        Some(target) => *dir = crate::moved(dir, &native_path(target), home),
    }
}

/// Git Bash's `/c/Users/...` as `C:/Users/...` on Windows, where Claude Code
/// runs its Bash tool in Git Bash.
fn native_path(target: &str) -> String {
    if cfg!(windows) {
        let mut chars = target.chars();
        if let (Some('/'), Some(drive), None | Some('/')) =
            (chars.next(), chars.next(), chars.next())
            && drive.is_ascii_alphabetic()
        {
            return format!("{}:/{}", drive.to_ascii_uppercase(), chars.as_str());
        }
    }
    target.to_owned()
}
