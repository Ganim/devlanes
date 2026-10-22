//! In-memory environment precedence and provenance, independent of config loading.
//!
//! Environment maps are `BTreeMap<String, _>` keyed by variable name. Windows
//! treats names case-insensitively (`Path` and `PATH` are one variable), so code
//! that merges or looks up variables goes through [`insert`], [`get`],
//! [`contains_key`] and [`remove`] instead of the map methods. On Unix they are
//! the plain map operations.

use std::collections::BTreeMap;

pub use os::other_spelling;

/// Looks up `name`, matching another spelling on Windows (`Path` for `PATH`).
pub fn get<'a, V>(map: &'a BTreeMap<String, V>, name: &str) -> Option<&'a V> {
    map.get(name)
        .or_else(|| other_spelling(map.keys(), name).and_then(|key| map.get(key)))
}

/// Whether `name` is set, under any spelling on Windows.
pub fn contains_key<V>(map: &BTreeMap<String, V>, name: &str) -> bool {
    get(map, name).is_some()
}

/// Sets `name`, replacing the variable with the same name. On Windows a
/// differently spelled entry is replaced in place and keeps its spelling, so a
/// later layer's `PATH` overrides the client's `Path` instead of sitting next to it.
pub fn insert<V>(map: &mut BTreeMap<String, V>, name: String, value: V) -> Option<V> {
    let key = other_spelling(map.keys(), &name).cloned().unwrap_or(name);
    map.insert(key, value)
}

/// Removes `name` under every spelling Windows treats as the same variable.
/// Returns the value of the last entry removed.
pub fn remove<V>(map: &mut BTreeMap<String, V>, name: &str) -> Option<V> {
    let mut removed = map.remove(name);
    while let Some(key) = other_spelling(map.keys(), name).cloned() {
        removed = map.remove(&key);
    }
    removed
}

#[cfg(unix)]
mod os {
    /// The key among `keys` that names the same variable as `name` but is
    /// spelled differently. Unix names are case-sensitive, so there is none.
    pub fn other_spelling<'a>(
        _keys: impl IntoIterator<Item = &'a String>,
        _name: &str,
    ) -> Option<&'a String> {
        None
    }
}

#[cfg(windows)]
mod os {
    /// The key among `keys` that names the same variable as `name` but is
    /// spelled differently. Windows compares names ignoring case; ASCII folding
    /// covers real variable names (non-ASCII names are compared exactly).
    pub fn other_spelling<'a>(
        keys: impl IntoIterator<Item = &'a String>,
        name: &str,
    ) -> Option<&'a String> {
        keys.into_iter()
            .find(|key| key.as_str() != name && key.eq_ignore_ascii_case(name))
    }
}

/// Layers are ordered from the lowest to highest precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EnvLayer {
    Client,
    Contributor,
    Secrets,
    Devlanes,
    Global,
    Service,
    Profile,
    Override,
}

/// Effective environment with provenance. No serialization or debug implementation
/// is provided because both values and overridden values may contain secrets.
#[derive(Default)]
pub struct Environment {
    pub values: BTreeMap<String, String>,
    pub sources: BTreeMap<String, EnvLayer>,
}

/// Assembles layers by precedence regardless of caller order. Within a layer,
/// later entries win. Dotenv values belong to expression evaluation, not this API.
/// On Windows a higher layer replaces a variable spelled differently in a lower
/// one (see [`insert`]).
pub fn assemble(
    layers: impl IntoIterator<Item = (EnvLayer, BTreeMap<String, String>)>,
) -> Environment {
    let mut layers: Vec<_> = layers.into_iter().collect();
    layers.sort_by_key(|(layer, _)| *layer);
    let mut result = Environment::default();
    for (layer, values) in layers {
        for (key, value) in values {
            // Both maps hold the same keys, so both resolve the same spelling.
            insert(&mut result.sources, key.clone(), layer);
            insert(&mut result.values, key, value);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Caller iteration order cannot invert the documented environment precedence.
    fn precedence_is_independent_of_input_order() {
        let env = assemble([
            (
                EnvLayer::Override,
                BTreeMap::from([("PORT".into(), "3".into())]),
            ),
            (
                EnvLayer::Client,
                BTreeMap::from([("PORT".into(), "1".into())]),
            ),
            (
                EnvLayer::Service,
                BTreeMap::from([("PORT".into(), "2".into())]),
            ),
        ]);
        assert_eq!(env.values["PORT"], "3");
        assert_eq!(env.sources["PORT"], EnvLayer::Override);
    }

    #[cfg(unix)]
    #[test]
    /// Unix names are case-sensitive: `Path` and `PATH` stay two variables.
    fn unix_names_are_case_sensitive() {
        let env = assemble([
            (
                EnvLayer::Client,
                BTreeMap::from([("Path".into(), "client".into())]),
            ),
            (
                EnvLayer::Contributor,
                BTreeMap::from([("PATH".into(), "mise".into())]),
            ),
        ]);
        assert_eq!(env.values.len(), 2);
        assert_eq!(get(&env.values, "PATH").map(String::as_str), Some("mise"));
        assert_eq!(get(&env.values, "path"), None);
    }

    #[cfg(windows)]
    #[test]
    /// A later layer's `PATH` replaces the client's `Path` in place: one variable,
    /// the first spelling, the later value and layer.
    fn windows_layers_merge_names_ignoring_case() {
        let env = assemble([
            (
                EnvLayer::Client,
                BTreeMap::from([
                    ("Path".into(), "client".into()),
                    ("SystemRoot".into(), r"C:\Windows".into()),
                ]),
            ),
            (
                EnvLayer::Contributor,
                BTreeMap::from([("PATH".into(), "mise".into())]),
            ),
        ]);
        assert_eq!(env.values.len(), 2);
        assert_eq!(env.values["Path"], "mise");
        assert_eq!(env.sources["Path"], EnvLayer::Contributor);
        assert_eq!(get(&env.values, "PATH").map(String::as_str), Some("mise"));
        assert_eq!(
            get(&env.values, "SYSTEMROOT").map(String::as_str),
            Some(r"C:\Windows")
        );
    }

    #[cfg(windows)]
    #[test]
    /// Lookups and removal on Windows find every spelling of a name.
    fn windows_lookup_and_removal_ignore_case() {
        let mut map = BTreeMap::from([
            ("Path".to_owned(), "a".to_owned()),
            ("PATH".to_owned(), "b".to_owned()),
            ("FORCE_COLOR".to_owned(), "1".to_owned()),
        ]);
        assert!(contains_key(&map, "force_color"));
        assert!(remove(&mut map, "path").is_some());
        assert_eq!(map.len(), 1);
        insert(&mut map, "Force_Color".into(), "0".into());
        assert_eq!(map["FORCE_COLOR"], "0");
    }
}
