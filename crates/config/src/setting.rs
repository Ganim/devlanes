//! User config settings addressed by dotted key (`tui.theme`,
//! `proxy.https_port`), for `devlanes config get|set|unset` and the TUI's
//! options page.

use serde_json::Value;

use crate::{ConfigError, TerminalConfig, UserConfig};

/// When a saved change to a user config setting takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// A running TUI redraws with it (colors, glyphs); every later command reads it too.
    Now,
    /// The next devlanes command reads it.
    NextCommand,
    /// The daemon reads it when it starts, so it needs `devlanes daemon restart`.
    Daemon,
}

/// When a change to the dotted user config `key` takes effect. `tui` and
/// `terminal` are read by each command; everything else by the daemon.
pub fn applies(key: &str) -> Applies {
    let section = key.split('.').next().unwrap_or(key);
    match key {
        "tui.theme" | "tui.glyphs" => Applies::Now,
        _ if matches!(section, "tui" | "terminal") => Applies::NextCommand,
        _ => Applies::Daemon,
    }
}

/// Every section and setting of the user config as a dotted key (`proxy`,
/// `proxy.https_port`), in schema order.
pub fn user_keys() -> Vec<String> {
    let schema = crate::user_schema();
    let mut keys = Vec::new();
    collect(&schema, &schema, "", &mut keys);
    keys
}

/// Fails unless `key` names a section or setting of the user config, with
/// the closest known key when it looks like a typo.
pub fn check_user_key(key: &str) -> Result<(), ConfigError> {
    let keys = user_keys();
    if keys.iter().any(|known| known == key) {
        return Ok(());
    }
    // A typo keeps its depth: `tui.them` means a setting, not the `tui` section.
    let depth = key.split('.').count();
    let candidates = keys
        .iter()
        .filter(|known| known.split('.').count() == depth)
        .map(String::as_str);
    Err(ConfigError::plain(format!(
        "unknown setting `{key}`{}",
        crate::loader::did_you_mean(key, candidates)
    )))
}

/// The effective user config as plain data, every default included. An unset
/// `terminal`, or the app name older versions used, reads as its default
/// settings, since that is what devlanes uses. Durations and sizes use their
/// largest exact unit (`7d`, `5GiB`).
pub fn effective_user_value(config: &UserConfig) -> Value {
    let mut value = serde_json::to_value(config).unwrap_or_default();
    let terminal = config
        .terminal
        .as_ref()
        .map(TerminalConfig::settings)
        .unwrap_or_default();
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "terminal".into(),
            serde_json::to_value(terminal).unwrap_or_default(),
        );
    }
    shorten(&mut value);
    value
}

/// Rewrites the lossless forms durations and sizes serialize to
/// (`604800000ms`, `5368709120B`) in their largest exact unit. No other user
/// setting holds a string of digits followed by `ms` or `B`.
fn shorten(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Some(short) = short_duration(text).or_else(|| short_size(text)) {
                *text = short;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(shorten),
        Value::Object(map) => map.values_mut().for_each(shorten),
        _ => {}
    }
}

/// `20000ms` as `20s`; `None` for anything else.
fn short_duration(text: &str) -> Option<String> {
    let millis: u64 = digits(text.strip_suffix("ms")?)?;
    let unit = [
        ("d", 86_400_000),
        ("h", 3_600_000),
        ("m", 60_000),
        ("s", 1000),
    ]
    .into_iter()
    .find(|(_, size)| millis != 0 && millis.is_multiple_of(*size));
    Some(match unit {
        Some((name, size)) => format!("{}{name}", millis / size),
        None => format!("{millis}ms"),
    })
}

/// `5368709120B` as `5GiB`; `None` for anything else.
fn short_size(text: &str) -> Option<String> {
    let bytes: u64 = digits(text.strip_suffix('B')?)?;
    let unit = [
        ("GiB", 1 << 30),
        ("GB", 1_000_000_000),
        ("MiB", 1 << 20),
        ("MB", 1_000_000),
        ("KiB", 1 << 10),
        ("KB", 1000),
    ]
    .into_iter()
    .find(|(_, size)| bytes != 0 && bytes.is_multiple_of(*size));
    Some(match unit {
        Some((name, size)) => format!("{}{name}", bytes / size),
        None => format!("{bytes}B"),
    })
}

/// A whole number written only with ASCII digits.
fn digits(text: &str) -> Option<u64> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// The effective value of the dotted `key` in `config`, defaults included.
/// Null for a setting that is unset and has no default (`startup.max`).
/// Fails like [`check_user_key`] for an unknown key.
pub fn user_setting(config: &UserConfig, key: &str) -> Result<Value, ConfigError> {
    check_user_key(key)?;
    let effective = effective_user_value(config);
    Ok(key
        .split('.')
        .try_fold(&effective, |node, segment| node.get(segment))
        .cloned()
        .unwrap_or(Value::Null))
}

/// A setting's value typed on the command line, read as YAML: `true`, `443`,
/// `dark`, `30d`, `[system, firefox]` or `{progress: never}`.
pub fn parse_setting_value(text: &str) -> Result<Value, ConfigError> {
    crate::loader::parse_yaml("value", text)
}

/// `value` as YAML text ending in a newline, the way config.yaml would hold it.
pub fn to_yaml(value: &Value) -> Result<String, ConfigError> {
    let mut text =
        serde_saphyr::to_string(value).map_err(|error| ConfigError::plain(error.to_string()))?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// Adds the keys under `node`'s properties.
fn collect(node: &Value, root: &Value, prefix: &str, out: &mut Vec<String>) {
    for (name, property) in properties(node, root) {
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if out.contains(&key) {
            continue;
        }
        out.push(key.clone());
        collect(property, root, &key, out);
    }
}

/// The properties of a schema node, through `$ref`s and every `anyOf`
/// alternative (`terminal` is a mapping or a string).
fn properties<'a>(node: &'a Value, root: &'a Value) -> Vec<(&'a String, &'a Value)> {
    let node = resolve(node, root);
    let mut out: Vec<_> = node
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .collect();
    for branch in node
        .get("anyOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        out.extend(properties(branch, root));
    }
    out
}

/// Follows `$ref`s to the definition.
fn resolve<'a>(mut node: &'a Value, root: &'a Value) -> &'a Value {
    for _ in 0..8 {
        match node
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix('#'))
            .and_then(|pointer| root.pointer(pointer))
        {
            Some(target) => node = target,
            None => break,
        }
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_cover_sections_settings_and_both_terminal_forms() {
        let keys = user_keys();
        for key in [
            "tui",
            "tui.theme",
            "proxy.https_port",
            "terminal",
            "terminal.progress",
            "tls.trust",
            "startup.max",
        ] {
            assert!(keys.iter().any(|k| k == key), "{key}: {keys:?}");
        }
        assert!(check_user_key("proxy.https_port").is_ok());
        assert_eq!(
            check_user_key("proxy.htps_port").expect_err("typo").message,
            "unknown setting `proxy.htps_port`; did you mean `proxy.https_port`?"
        );
        assert_eq!(
            check_user_key("zzz").expect_err("unknown").message,
            "unknown setting `zzz`"
        );
        // A section is no suggestion for a mistyped setting.
        assert!(
            !check_user_key("tui.colr")
                .expect_err("unknown")
                .message
                .contains("`tui`")
        );
    }

    #[test]
    fn durations_and_sizes_read_in_their_largest_unit() {
        let whole = effective_user_value(&UserConfig::default());
        assert_eq!(whole["metrics"]["retention"], "7d");
        assert_eq!(whole["startup"]["slot_timeout"], "20s");
        assert_eq!(whole["logs"]["retention"]["max_size"], "5GiB");
        assert_eq!(whole["state"]["max_size"], "512MiB");
        assert_eq!(short_duration("1500ms").as_deref(), Some("1500ms"));
        assert_eq!(short_duration("0ms").as_deref(), Some("0ms"));
        assert_eq!(short_size("5000000000B").as_deref(), Some("5GB"));
        assert_eq!(short_size("dark"), None);
        assert_eq!(short_duration("ams"), None);
        // What `get` prints, `set` takes back.
        let config = crate::parse_user("metrics:\n  retention: 7d\nstate:\n  max_size: 5GB\n")
            .expect("valid");
        let value = effective_user_value(&config);
        assert_eq!(
            (&value["metrics"]["retention"], &value["state"]["max_size"]),
            (&json!("7d"), &json!("5GB"))
        );
    }

    #[test]
    fn effective_values_include_defaults() {
        let config = crate::parse_user("tui:\n  theme: dark\nterminal: ghostty\n").expect("valid");
        assert_eq!(user_setting(&config, "tui.theme").expect("known"), "dark");
        assert_eq!(user_setting(&config, "tui.glyphs").expect("known"), "auto");
        assert_eq!(
            user_setting(&config, "terminal.progress").expect("known"),
            "auto"
        );
        assert_eq!(
            user_setting(&config, "startup.max").expect("known"),
            Value::Null
        );
        assert!(user_setting(&config, "tui.colour").is_err());
        let whole = effective_user_value(&UserConfig::default());
        assert_eq!(whole["proxy"]["https_port"], json!(443));
        assert!(whole["terminal"].is_object());
    }

    #[test]
    fn typed_values_read_as_yaml() {
        assert_eq!(parse_setting_value("true").expect("bool"), json!(true));
        assert_eq!(parse_setting_value("443").expect("number"), json!(443));
        assert_eq!(parse_setting_value("dark").expect("string"), json!("dark"));
        assert_eq!(parse_setting_value("30d").expect("string"), json!("30d"));
        assert_eq!(
            parse_setting_value("[system, firefox]").expect("list"),
            json!(["system", "firefox"])
        );
        assert_eq!(
            parse_setting_value("'true'").expect("quoted"),
            json!("true")
        );
        assert!(parse_setting_value("[unclosed").is_err());
    }

    #[test]
    fn values_print_as_yaml() {
        assert_eq!(to_yaml(&json!("dark")).expect("yaml"), "dark\n");
        assert_eq!(to_yaml(&json!(443)).expect("yaml"), "443\n");
        assert_eq!(
            to_yaml(&json!({"theme": "dark", "glyphs": "auto"})).expect("yaml"),
            "theme: dark\nglyphs: auto\n"
        );
    }

    #[test]
    fn tui_and_terminal_apply_without_the_daemon() {
        assert_eq!(applies("tui.theme"), Applies::Now);
        assert_eq!(applies("tui.glyphs"), Applies::Now);
        assert_eq!(applies("tui.open_on_start"), Applies::NextCommand);
        assert_eq!(applies("terminal.progress"), Applies::NextCommand);
        assert_eq!(applies("terminal"), Applies::NextCommand);
        assert_eq!(applies("proxy.https_port"), Applies::Daemon);
        assert_eq!(applies("tuix"), Applies::Daemon);
    }
}
