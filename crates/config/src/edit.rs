//! Targeted edits of the user config file that keep its comments, key order
//! and formatting. The file is never rewritten from [`crate::UserConfig`].

use std::{path::Path, str::FromStr};

use serde_json::Value;
use yaml_edit::{Document, YamlFile, YamlValue, path::YamlPath};

use crate::ConfigError;

/// Sets the dotted `key` (`tui.theme`, `proxy.https_port`) of the user config
/// at `path` to `value`, creating the file and missing sections. Everything
/// else in the file stays byte for byte. Fails without writing when the value
/// does not parse or validate, naming the setting in the message.
pub fn set_user_value(path: &Path, key: &str, value: &Value) -> Result<(), ConfigError> {
    rewrite(path, |source| edit_user_source(source, key, Some(value)))
}

/// Removes the dotted `key` from the user config at `path`, so its default
/// applies again; sections left empty go too. A missing key or file is fine.
pub fn remove_user_value(path: &Path, key: &str) -> Result<(), ConfigError> {
    rewrite(path, |source| edit_user_source(source, key, None))
}

/// [`set_user_value`] (`Some`) or [`remove_user_value`] (`None`) on the text of
/// a user config, returning the new text. Rejects edits that would add a
/// problem to the file, such as a value of the wrong type, an unknown key or a
/// failed cross-field check.
pub fn edit_user_source(
    source: &str,
    key: &str,
    value: Option<&Value>,
) -> Result<String, ConfigError> {
    let segments: Vec<&str> = key.split('.').collect();
    if segments.iter().any(|segment| !is_plain_key(segment)) {
        return Err(ConfigError::plain(format!("{key}: not a setting name")));
    }
    let before = parsed(source)?;
    let mut expected = before.clone();
    let changed = match value {
        Some(value) => place(&mut expected, &segments, value.clone()),
        None => take(&mut expected, &segments),
    };
    if !changed {
        return Ok(source.to_owned());
    }
    let edited = edit_text(source, key, &segments, value)?;
    // The tree edit must change exactly this key, nothing else.
    if parsed(&edited)? != expected {
        return Err(ConfigError::plain(format!(
            "{key}: cannot be written without changing other settings; edit config.yaml by hand"
        )));
    }
    let known: Vec<String> = problems(source);
    if let Some(problem) = problems(&edited)
        .into_iter()
        .find(|problem| !known.contains(problem))
    {
        return Err(ConfigError::plain(problem));
    }
    Ok(edited)
}

/// Reads `path` (absent is empty), applies `edit` and writes the result
/// through a temporary file, so a crash never leaves half a config.
fn rewrite(
    path: &Path,
    edit: impl FnOnce(&str) -> Result<String, ConfigError>,
) -> Result<(), ConfigError> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(ConfigError::io(path, error)),
    };
    let edited = edit(&source)?;
    if edited == source {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| ConfigError::io(parent, error))?;
    }
    let temporary = path.with_extension("yaml.tmp");
    std::fs::write(&temporary, edited).map_err(|error| ConfigError::io(&temporary, error))?;
    std::fs::rename(&temporary, path).map_err(|error| ConfigError::io(path, error))
}

/// Setting names are plain words; anything else could address YAML the
/// path syntax treats specially (`[0]`, quotes).
fn is_plain_key(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// User config text as plain data, the way the loader reads it; an empty
/// file is an empty mapping. Nothing is validated.
pub fn parse_user_value(source: &str) -> Result<Value, ConfigError> {
    parsed(source)
}

/// The file as the loader sees it; an empty file is an empty mapping.
fn parsed(source: &str) -> Result<Value, ConfigError> {
    let value: Value = crate::loader::parse_yaml("config.yaml", source)?;
    Ok(if value.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        value
    })
}

/// The messages of every setting a lenient load would drop, without the
/// "the default applies" tail.
fn problems(source: &str) -> Vec<String> {
    crate::parse_user_lenient("config.yaml", source)
        .1
        .into_iter()
        .map(|error| {
            error
                .message
                .trim_end_matches("; the default applies")
                .to_owned()
        })
        .collect()
}

/// Sets `segments` in a JSON tree, creating objects on the way; false when
/// the value is already there.
fn place(tree: &mut Value, segments: &[&str], value: Value) -> bool {
    let mut node = tree;
    for segment in &segments[..segments.len() - 1] {
        if !node.is_object() {
            *node = Value::Object(serde_json::Map::new());
        }
        let Value::Object(map) = node else {
            return false;
        };
        node = map
            .entry((*segment).to_owned())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
    if !node.is_object() {
        *node = Value::Object(serde_json::Map::new());
    }
    let Value::Object(map) = node else {
        return false;
    };
    let last = segments[segments.len() - 1].to_owned();
    if map.get(&last) == Some(&value) {
        return false;
    }
    map.insert(last, value);
    true
}

/// Removes `segments` from a JSON tree and the objects it leaves empty;
/// false when the key is not there.
fn take(tree: &mut Value, segments: &[&str]) -> bool {
    let Value::Object(map) = tree else {
        return false;
    };
    let Some((first, rest)) = segments.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return map.remove(*first).is_some();
    }
    let Some(child) = map.get_mut(*first) else {
        return false;
    };
    let removed = take(child, rest);
    if removed && child.as_object().is_some_and(serde_json::Map::is_empty) {
        map.remove(*first);
    }
    removed
}

/// The tree edit itself.
fn edit_text(
    source: &str,
    key: &str,
    segments: &[&str],
    value: Option<&Value>,
) -> Result<String, ConfigError> {
    let failed = |reason: String| ConfigError::plain(format!("{key}: {reason}"));
    let file = YamlFile::from_str(source).map_err(|error| failed(error.to_string()))?;
    let Some(value) = value else {
        let document = file.document().ok_or_else(|| failed("not set".into()))?;
        document
            .try_remove_path(key)
            .map_err(|error| failed(error.to_string()))?;
        // An emptied section would read as null, which is not a valid section.
        for depth in (1..segments.len()).rev() {
            let parent = segments[..depth].join(".");
            let empty = document
                .try_get_path(&parent)
                .ok()
                .and_then(|node| node.as_mapping().map(|mapping| mapping.is_empty()));
            if empty == Some(true) {
                document
                    .try_remove_path(&parent)
                    .map_err(|error| failed(error.to_string()))?;
            }
        }
        // yaml-edit writes an emptied root as `{}`; a file of comments reads better.
        if document
            .as_mapping()
            .is_some_and(|mapping| mapping.is_empty())
        {
            return Ok(file
                .to_string()
                .lines()
                .filter(|line| line.trim() != "{}")
                .map(|line| format!("{line}\n"))
                .collect());
        }
        return Ok(file.to_string());
    };
    let yaml = yaml_value(value).ok_or_else(|| failed("cannot be written as YAML".into()))?;
    match file.document() {
        // A file with only comments has no document; the new one goes after them.
        Some(document) if document.as_mapping().is_some() => {
            document
                .try_set_path(key, &yaml)
                .map_err(|error| failed(error.to_string()))?;
            Ok(file.to_string())
        }
        Some(_) => Err(failed("config.yaml is not a mapping of settings".into())),
        None => {
            let document = Document::new_mapping();
            document
                .try_set_path(key, &yaml)
                .map_err(|error| failed(error.to_string()))?;
            let mut text = source.to_owned();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            let body = document.to_string();
            text.push_str(body.strip_prefix("---\n").unwrap_or(&body));
            Ok(text)
        }
    }
}

/// JSON to the YAML value yaml-edit writes; `None` for numbers YAML cannot
/// hold exactly.
fn yaml_value(value: &Value) -> Option<YamlValue> {
    Some(match value {
        Value::Null => YamlValue::scalar(yaml_edit::ScalarValue::null()),
        Value::Bool(flag) => YamlValue::from(*flag),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => YamlValue::from(integer),
            None => YamlValue::from(number.as_f64()?),
        },
        Value::String(text) => YamlValue::from(text.as_str()),
        Value::Array(items) => {
            YamlValue::Sequence(items.iter().map(yaml_value).collect::<Option<_>>()?)
        }
        Value::Object(map) => YamlValue::Mapping(
            map.iter()
                .map(|(key, value)| Some((key.clone(), yaml_value(value)?)))
                .collect::<Option<_>>()?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COMMENTED: &str = "\
# devlanes machine settings
# see https://devlanes.run/docs/config

proxy:
  # 443 needs the helper on macOS
  https_port: 443   # standard port
  http_port: 80
  lan: false

tui:
  theme: auto # follow the terminal
  glyphs: auto

# keep a month of history
history:
  retention: 30d
";

    fn set(source: &str, key: &str, value: Value) -> String {
        edit_user_source(source, key, Some(&value)).expect("edit")
    }

    #[test]
    fn three_edits_keep_comments_and_order() {
        let mut text = set(COMMENTED, "proxy.https_port", json!(8443));
        text = set(&text, "tui.theme", json!("dark"));
        text = set(&text, "history.retention", json!("14d"));
        let expected = COMMENTED
            .replace("https_port: 443", "https_port: 8443")
            .replace("theme: auto", "theme: dark")
            .replace("retention: 30d", "retention: 14d");
        assert_eq!(text, expected);
    }

    #[test]
    fn missing_keys_join_their_section_and_new_sections_go_last() {
        let text = set(COMMENTED, "tui.open_on_start", json!(false));
        let config = crate::parse_user(&text).expect("valid");
        assert!(!config.tui.open_on_start);
        assert!(text.starts_with("# devlanes machine settings\n"));
        assert!(text.contains("  theme: auto # follow the terminal\n"));
        // The new key sits in the tui section, before the history comment.
        let tui = text.find("tui:").expect("tui");
        let added = text.find("open_on_start").expect("added");
        let history = text.find("# keep a month").expect("history");
        assert!(tui < added && added < history, "{text}");

        let text = set(COMMENTED, "startup.max", json!(6));
        assert!(text.starts_with(COMMENTED.trim_end()), "{text}");
        assert_eq!(
            crate::parse_user(&text).expect("valid").startup.max,
            std::num::NonZeroU32::new(6)
        );
    }

    #[test]
    fn empty_and_comment_only_files() {
        assert_eq!(
            set("", "tui.theme", json!("light")),
            "tui:\n  theme: light\n"
        );
        let text = set("# mine\n", "updates.check", json!(false));
        assert!(text.starts_with("# mine\n"), "{text}");
        assert!(!crate::parse_user(&text).expect("valid").updates.check);
    }

    #[test]
    fn removing_restores_the_default_and_drops_empty_sections() {
        let text = edit_user_source(COMMENTED, "history.retention", None).expect("edit");
        assert!(!text.contains("history:"), "{text}");
        assert!(text.contains("# devlanes machine settings"));
        let text = edit_user_source(COMMENTED, "tui.glyphs", None).expect("edit");
        assert!(text.contains("  theme: auto # follow the terminal\n"));
        assert!(!text.contains("glyphs"));
        // Removing the last setting leaves the comments, not an empty `{}`.
        let text = edit_user_source("# mine\ntui:\n  theme: dark\n", "tui.theme", None);
        assert_eq!(text.expect("edit"), "# mine\n");
        // Absent keys are already at their default.
        assert_eq!(
            edit_user_source(COMMENTED, "startup.max", None).expect("edit"),
            COMMENTED
        );
    }

    #[test]
    fn values_that_do_not_parse_or_validate_are_refused() {
        let wrong_type = edit_user_source(COMMENTED, "proxy.https_port", Some(&json!("x")));
        assert!(
            wrong_type
                .expect_err("rejected")
                .message
                .starts_with("proxy.https_port:")
        );
        let duration = edit_user_source(COMMENTED, "history.retention", Some(&json!("soon")));
        assert!(
            duration
                .expect_err("rejected")
                .message
                .contains("invalid duration")
        );
        // Cross-field checks name the field they are about.
        let same = edit_user_source(COMMENTED, "proxy.http_port", Some(&json!(443)));
        assert_eq!(
            same.expect_err("rejected").message,
            "proxy.https_port: must differ from proxy.http_port"
        );
        let unknown = edit_user_source(COMMENTED, "tui.colour", Some(&json!("x")));
        assert!(unknown.is_err());
    }

    #[test]
    fn existing_problems_do_not_block_other_edits() {
        let broken = "proxy:\n  https_port: nope\n";
        let text = set(broken, "tui.theme", json!("dark"));
        assert!(text.starts_with(broken), "{text}");
    }

    #[test]
    fn flow_sections_and_files_without_a_final_newline() {
        let text = set(
            "tui: {theme: auto, glyphs: auto}",
            "tui.theme",
            json!("light"),
        );
        assert_eq!(text, "tui: {theme: light, glyphs: auto}");
        let text = set("tui: {theme: auto}", "tui.glyphs", json!("ascii"));
        let config = crate::parse_user(&text).expect("valid");
        assert_eq!(config.tui.glyphs, crate::TuiGlyphs::Ascii);
        let text = set("updates:\n  check: true", "updates.check", json!(false));
        assert_eq!(text, "updates:\n  check: false");
    }

    #[test]
    fn strings_that_look_like_other_types_are_quoted() {
        let text = set("", "hostnames.tld", json!("true"));
        let value: Value = crate::loader::parse_yaml("t", &text).expect("yaml");
        assert_eq!(value["hostnames"]["tld"], json!("true"));
    }

    #[test]
    fn lists_and_mappings() {
        let text = set(COMMENTED, "tls.trust", json!(["system", "firefox"]));
        let config = crate::parse_user(&text).expect("valid");
        assert_eq!(config.tls.trust.len(), 2);
        let text = set(
            "terminal: ghostty # mine\n",
            "terminal",
            json!({"progress": "never"}),
        );
        let config = crate::parse_user(&text).expect("valid");
        assert_eq!(
            config.terminal.expect("terminal").progress(),
            crate::TerminalProgress::Never
        );
    }

    #[test]
    fn files_on_disk_are_created_and_edited_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("home/config.yaml");
        set_user_value(&path, "tui.theme", &json!("dark")).expect("created");
        std::fs::write(
            &path,
            format!("# top\n{}", std::fs::read_to_string(&path).expect("read")),
        )
        .expect("write");
        set_user_value(&path, "tui.glyphs", &json!("ascii")).expect("edited");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text, "# top\ntui:\n  theme: dark\n  glyphs: ascii\n");
        remove_user_value(&path, "tui.theme").expect("removed");
        remove_user_value(&path, "tui.theme").expect("already gone");
        assert!(set_user_value(&path, "proxy.https_port", &json!(-1)).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "# top\ntui:\n  glyphs: ascii\n"
        );
    }
}
