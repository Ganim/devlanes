/// Normalized severity ordering used by minimum-level filters.
pub(crate) fn rank(level: &str) -> Option<u8> {
    match level.to_ascii_lowercase().as_str() {
        "trace" | "silly" | "10" => Some(0),
        "debug" | "verbose" | "20" => Some(1),
        "info" | "log" | "notice" | "http" | "30" => Some(2),
        "warn" | "warning" | "40" => Some(3),
        "error" | "err" | "50" => Some(4),
        "fatal" | "panic" | "critical" | "crit" | "60" => Some(5),
        _ => None,
    }
}

/// Structured fields take precedence over incidental severity words in a message.
pub fn extract_level(text: &str) -> Option<String> {
    let fields = [
        "level",
        "lvl",
        "severity",
        "levelname",
        "log.level",
        "@l",
        "PRIORITY",
    ];
    if let Some(start) = text.find('{')
        && let Ok(serde_json::Value::Object(object)) = serde_json::from_str(&text[start..])
    {
        for key in fields {
            if let Some(value) = object.get(key) {
                let value = value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string());
                if let Some(level) = field_level(key, &value) {
                    return Some(level);
                }
            }
        }
        if let Some(value) = object
            .get("log")
            .and_then(|v| v.get("level"))
            .and_then(|v| v.as_str())
        {
            return field_level("level", value);
        }
        return None;
    }
    // Split logfmt outside quoted values so `msg="level=error"` is never a level.
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        if escaped {
            escaped = false;
        } else if c == '\\' && quoted {
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
        } else if c.is_whitespace() && !quoted {
            if let Some((key, value)) = text[start..index].split_once('=')
                && fields.contains(&key)
                && let Some(level) = field_level(key, value.trim_matches('"'))
            {
                return Some(level);
            }
            start = index + c.len_utf8();
        }
    }
    text.split_whitespace().take(4).find_map(|token| {
        let token = token.trim_matches(|c: char| !c.is_ascii_alphabetic());
        // Plain text numeric timestamps must not become pino levels.
        if token.is_empty() {
            None
        } else {
            field_level("level", token)
        }
    })
}

/// systemd's priority scale runs in the opposite direction to pino's scale.
fn field_level(key: &str, value: &str) -> Option<String> {
    let rank = if key == "PRIORITY" {
        match value.parse::<u8>().ok()? {
            0..=2 => 5,
            3 => 4,
            4 => 3,
            5..=6 => 2,
            7 => 1,
            _ => return None,
        }
    } else {
        rank(value)?
    };
    Some(["trace", "debug", "info", "warn", "error", "fatal"][usize::from(rank)].to_owned())
}
