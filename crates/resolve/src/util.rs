use serde_json::Value;

/// Quotes `value` for a POSIX `sh` (`/bin/sh`, Git for Windows' `sh.exe`) when
/// it contains anything but safe characters.
///
/// ```
/// assert_eq!(devlanes_resolve::shell_quote("--port"), "--port");
/// assert_eq!(devlanes_resolve::shell_quote("it's"), r"'it'\''s'");
/// ```
pub fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
    if safe {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Quotes `value` as one argument on a `cmd.exe /d /s /c` line, or `None` when
/// it cannot be passed safely: `"` cannot be escaped inside quotes, `%` expands
/// variables even inside quotes, `!` does when delayed expansion is on, and a
/// line break ends the command.
///
/// ```
/// assert_eq!(devlanes_resolve::cmd_quote("--port").as_deref(), Some("--port"));
/// assert_eq!(devlanes_resolve::cmd_quote(r"a b\").as_deref(), Some(r#""a b\\""#));
/// assert_eq!(devlanes_resolve::cmd_quote("100%"), None);
/// ```
pub fn cmd_quote(value: &str) -> Option<String> {
    if value.contains(['"', '%', '!', '\r', '\n', '\0']) {
        return None;
    }
    // `=` and `,` split batch-file parameters (npm.cmd and friends), so they
    // get quotes too.
    let safe = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@+:./-".contains(&b));
    if safe {
        return Some(value.to_owned());
    }
    // Inside quotes cmd.exe leaves `&|<>^()` alone; the program then splits its
    // command line with the MSVC rules, where backslashes before the closing
    // quote must be doubled or they escape it.
    let trailing = value.len() - value.trim_end_matches('\\').len();
    Some(format!("\"{value}{}\"", "\\".repeat(trailing)))
}

/// Returns `DEVLANES_PUBLIC_URL_<SERVICE>`: uppercase, every character other than
/// ASCII letters and digits becomes `_`.
///
/// ```
/// assert_eq!(devlanes_resolve::public_url_env_key("crm-api"), "DEVLANES_PUBLIC_URL_CRM_API");
/// ```
pub fn public_url_env_key(service: &str) -> String {
    let suffix: String = service
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("DEVLANES_PUBLIC_URL_{suffix}")
}

/// Maps the stop signals accepted by config validation to numbers. SIGUSR1/2
/// differ between macOS and Linux, so they are compile-time per OS.
pub(crate) fn signal_number(name: &str) -> Option<i32> {
    #[cfg(target_os = "linux")]
    const USR: (i32, i32) = (10, 12);
    #[cfg(not(target_os = "linux"))]
    const USR: (i32, i32) = (30, 31);
    Some(match name {
        "SIGHUP" => 1,
        "SIGINT" => 2,
        "SIGQUIT" => 3,
        "SIGKILL" => 9,
        "SIGTERM" => 15,
        "SIGUSR1" => USR.0,
        "SIGUSR2" => USR.1,
        _ => return None,
    })
}

/// Collects every string containing `${{` in a serialized config value.
pub(crate) fn expression_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) if text.contains("${{") => out.push(text.clone()),
        Value::Object(map) => map.values().for_each(|v| expression_strings(v, out)),
        Value::Array(values) => values.iter().for_each(|v| expression_strings(v, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Quoting leaves flags alone and protects spaces, quotes and empty strings.
    fn quoting() {
        assert_eq!(shell_quote("127.0.0.1"), "127.0.0.1");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    /// cmd.exe quoting protects spaces and operators, doubles trailing
    /// backslashes and refuses what quotes cannot protect.
    fn cmd_quoting() {
        assert_eq!(cmd_quote("127.0.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(cmd_quote("").as_deref(), Some(r#""""#));
        assert_eq!(cmd_quote("a & b").as_deref(), Some(r#""a & b""#));
        assert_eq!(cmd_quote("--host=::1").as_deref(), Some(r#""--host=::1""#));
        assert_eq!(cmd_quote(r"C:\dir\\").as_deref(), Some(r#""C:\dir\\\\""#));
        for unsafe_value in ["say \"hi\"", "%PATH%", "wow!", "a\nb"] {
            assert_eq!(cmd_quote(unsafe_value), None, "{unsafe_value:?}");
        }
    }

    #[test]
    /// Service names with dashes and dots map to one env key form.
    fn env_keys() {
        assert_eq!(public_url_env_key("api"), "DEVLANES_PUBLIC_URL_API");
        assert_eq!(public_url_env_key("a.b-c"), "DEVLANES_PUBLIC_URL_A_B_C");
    }

    #[test]
    /// Unknown signals are rejected rather than mapped to SIGTERM.
    fn signals() {
        assert_eq!(signal_number("SIGINT"), Some(2));
        assert_eq!(signal_number("SIGWINCH"), None);
    }
}
