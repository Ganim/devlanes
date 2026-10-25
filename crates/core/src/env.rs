//! In-memory environment precedence and provenance, independent of config loading.

use std::collections::BTreeMap;

/// Layers are ordered from the lowest to highest precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EnvLayer {
    Client,
    Contributor,
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
/// Names go through [`fold_name`], so on Windows `Path` from one layer and
/// `PATH` from another are one variable.
pub fn assemble(
    layers: impl IntoIterator<Item = (EnvLayer, BTreeMap<String, String>)>,
) -> Environment {
    let mut layers: Vec<_> = layers.into_iter().collect();
    layers.sort_by_key(|(layer, _)| *layer);
    let mut result = Environment::default();
    for (layer, values) in layers {
        for (key, value) in values {
            let key = fold_name(key);
            result.sources.insert(key.clone(), layer);
            result.values.insert(key, value);
        }
    }
    result
}

/// An environment variable name as devlanes keeps it: unchanged on Unix,
/// where names are case-sensitive.
#[cfg(unix)]
pub fn fold_name(name: String) -> String {
    name
}

/// An environment variable name as devlanes keeps it: uppercased on Windows,
/// where names ignore case, so `Path` and `PATH` are one key wherever
/// environments are captured, merged or looked up. The folding is
/// [`crate::paths::fold_case`]'s, which is how Windows compares names.
#[cfg(windows)]
pub fn fold_name(name: String) -> String {
    crate::paths::fold_case(&name)
}

/// `env` unchanged: Unix names are case-sensitive.
#[cfg(unix)]
pub fn fold_names(env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    env
}

/// `env` with every name through [`fold_name`]; when two names fold to one,
/// the value of the one sorting last wins.
#[cfg(windows)]
pub fn fold_names(env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.into_iter()
        .map(|(name, value)| (fold_name(name), value))
        .collect()
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

    #[test]
    /// Names keep their case on Unix and fold to uppercase on Windows, so a
    /// later layer's `PATH` replaces an earlier layer's `Path` there.
    fn names_fold_only_on_windows() {
        let env = assemble([
            (
                EnvLayer::Client,
                BTreeMap::from([("Path".into(), "client".into())]),
            ),
            (
                EnvLayer::Service,
                BTreeMap::from([("PATH".into(), "service".into())]),
            ),
        ]);
        let mixed = BTreeMap::from([("Path".to_owned(), "a".to_owned())]);
        #[cfg(unix)]
        {
            assert_eq!(env.values.len(), 2);
            assert_eq!(fold_name("Path".into()), "Path");
            assert_eq!(fold_names(mixed.clone()), mixed);
        }
        #[cfg(windows)]
        {
            assert_eq!(
                env.values,
                BTreeMap::from([("PATH".to_owned(), "service".to_owned())])
            );
            assert_eq!(env.sources["PATH"], EnvLayer::Service);
            assert_eq!(fold_name("ProgramFiles(x86)".into()), "PROGRAMFILES(X86)");
            assert_eq!(fold_name("straße".into()), "STRAßE");
            assert_eq!(
                fold_names(mixed),
                BTreeMap::from([("PATH".to_owned(), "a".to_owned())])
            );
        }
    }
}
