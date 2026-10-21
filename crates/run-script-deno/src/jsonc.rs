//! JSONC to JSON: drops comments and trailing commas, keeps line breaks so
//! parse errors still point at the right line.

/// Converts JSON with comments (`//`, `/* */`) and trailing commas to JSON.
pub(crate) fn to_json(text: &str) -> String {
    drop_trailing_commas(&drop_comments(text))
}

/// Removes comments outside strings. Newlines inside block comments are kept.
fn drop_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    if next == '\n' {
                        out.push('\n');
                    }
                    previous = next;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Removes a `,` outside strings when the next non-whitespace is `}` or `]`.
fn drop_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, &c) in chars.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == ','
            && chars[index + 1..]
                .iter()
                .find(|next| !next.is_whitespace())
                .is_some_and(|next| matches!(next, '}' | ']'))
        {
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Comments and trailing commas go; strings that look like them stay.
    #[test]
    fn strips_jsonc_syntax() {
        let text =
            "{\n  // line\n  \"a\": \"http://x/*y*/\", /* block\n spans */\n  \"b\": [1, 2,],\n}\n";
        let value: serde_json::Value = serde_json::from_str(&to_json(text)).expect("valid json");
        assert_eq!(
            value,
            serde_json::json!({"a": "http://x/*y*/", "b": [1, 2]})
        );
        assert_eq!(to_json(text).lines().count(), text.lines().count());
    }
}
