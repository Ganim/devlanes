//! Splitting command lines into words the way `sh` would, without expansion.

/// Splits `line` into words: spaces separate, `'...'` and `"..."` group, `\` escapes.
/// Returns `None` for an unterminated quote.
pub(crate) fn split(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\' | '$' | '`') => word.push(c),
                            c => {
                                word.push('\\');
                                word.push(c);
                            }
                        },
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

/// Quotes `word` for `sh` when it needs it.
pub(crate) fn quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Joins words into one `sh` command line.
pub(crate) fn join(words: &[String]) -> String {
    words
        .iter()
        .map(|word| quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_sh() {
        assert_eq!(
            split(r#"concurrently -n "api,web" 'npm:dev:*' a\ b"#),
            Some(vec![
                "concurrently".to_owned(),
                "-n".to_owned(),
                "api,web".to_owned(),
                "npm:dev:*".to_owned(),
                "a b".to_owned(),
            ])
        );
        assert_eq!(split("echo 'open"), None);
        assert_eq!(join(&["node".into(), "a b.js".into()]), "node 'a b.js'");
    }
}
