//! In-memory environment precedence and provenance, independent of config loading.

use std::collections::BTreeMap;

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
pub fn assemble(
    layers: impl IntoIterator<Item = (EnvLayer, BTreeMap<String, String>)>,
) -> Environment {
    let mut layers: Vec<_> = layers.into_iter().collect();
    layers.sort_by_key(|(layer, _)| *layer);
    let mut result = Environment::default();
    for (layer, values) in layers {
        for (key, value) in values {
            result.sources.insert(key.clone(), layer);
            result.values.insert(key, value);
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
}
