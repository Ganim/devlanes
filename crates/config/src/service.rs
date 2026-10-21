use crate::{
    BuiltinExtensions, ConfigDuration, Env, Extensions, FnoxConfig, HostConfig, ReadyConfig, Run,
    RunScript, SyncConfig, Templated,
};
use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{MapAccess, Visitor},
    ser::SerializeMap,
};
use serde_saphyr::Spanned;
use std::{fmt, marker::PhantomData};

/// `deserialize_any` prevents YAML null from being coerced to an empty mapping.
struct PresetMapping(serde_json::Map<String, serde_json::Value>);

impl<'de> Deserialize<'de> for PresetMapping {
    /// Reject every non-mapping YAML node at its original parser location.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MappingVisitor;
        impl<'de> Visitor<'de> for MappingVisitor {
            type Value = PresetMapping;
            /// Explain the required node shape in source-labelled YAML errors.
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a preset options mapping")
            }
            /// Preserve arbitrary integration fields until the registered parser runs.
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if values.insert(key.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate preset option `{key}`"
                        )));
                    }
                }
                Ok(PresetMapping(values))
            }
        }
        deserializer.deserialize_any(MappingVisitor)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Signal and grace period used when stopping a process tree.
#[schemars(extend("examples" = [{"signal":"SIGINT","timeout":"15s"}]))]
pub struct StopConfig {
    /// Initial stop signal; defaults to SIGTERM, followed by SIGKILL if the grace period expires.
    #[schemars(extend("examples" = ["SIGTERM","SIGINT"]))]
    pub signal: Option<String>,
    /// Grace period before SIGKILL; defaults to 10s.
    #[schemars(extend("examples" = ["15s","10m"]))]
    pub timeout: Option<Templated<ConfigDuration>>,
}

macro_rules! service_fields {
    ($($(#[$meta:meta])* $field:ident: $ty:ty),* $(,)?) => {
        /// Service settings after typed local and template merging.
        #[derive(Debug, Clone, JsonSchema)]
        #[schemars(bound = "E: Extensions", deny_unknown_fields)]
        #[schemars(extend("examples" = [{"run_script":"apps/api/package.json#dev","hosts":["api.${{ lane.domain }}"],"tags":["api"]}]))]
        pub struct ServiceConfig<E: Extensions = BuiltinExtensions> {
            $($(#[$meta])* pub $field: Option<$ty>,)*
            /// Service-specific environment overrides, above global_env.
            #[schemars(default)]
            #[schemars(extend("examples" = [{"API_URL":"${{ svc.api.url }}"}]))]
            pub env: Env,
            #[schemars(skip)]
            /// Options under the selected preset's name, validated by that integration.
            pub preset_options: IndexMap<String, E::Options>,
        }

        /// Optional source-bearing fields keep local overrides separate from defaults.
        #[derive(Debug, Clone)]
        pub(crate) struct PartialService<E: Extensions> {
            $(pub $field: Option<Spanned<$ty>>,)*
            pub env: Option<Spanned<Env>>,
            pub preset_options: IndexMap<String, Spanned<E::Options>>,
        }

        impl<E: Extensions> Default for PartialService<E> {
            /// Keep every field absent until base and local sources have been merged.
            fn default() -> Self {
                Self { $($field: None,)* env: None, preset_options: IndexMap::new() }
            }
        }

        impl<E: Extensions> Default for ServiceConfig<E> {
            /// Construct an empty template; runnable services require a command.
            fn default() -> Self { PartialService::<E>::default().finish() }
        }

        impl<E: Extensions> PartialService<E> {
            /// Template inheritance and local overlays replace service fields and preset blocks
            /// atomically so one-of settings such as ready and run/run_script cannot mix forms.
            /// Env entries merge per key; fnox retains unspecified fields.
            pub(crate) fn merge(&mut self, mut overlay: Self) {
                if overlay.run.is_some() { self.run_script = None; }
                if overlay.run_script.is_some() { self.run = None; }
                if let (Some(base), Some(local)) = (&self.fnox, &mut overlay.fnox) {
                    if local.value.profile.is_none() { local.value.profile = base.value.profile.clone(); }
                }
                $(if let Some(value) = overlay.$field { self.$field = Some(value); })*
                if let Some(overlay) = overlay.env {
                    if let Some(base) = &mut self.env { base.value.extend(overlay.value); }
                    else { self.env = Some(overlay); }
                }
                self.preset_options.extend(overlay.preset_options);
            }

            /// Drop span metadata only after all merge decisions have been made.
            pub(crate) fn finish(self) -> ServiceConfig<E> {
                ServiceConfig {
                    $($field: self.$field.map(|v| v.value),)*
                    env: self.env.map(|v| v.value).unwrap_or_default(),
                    preset_options: self.preset_options.into_iter().map(|(k,v)| (k,v.value)).collect(),
                }
            }
        }

        impl<'de, E: Extensions> Deserialize<'de> for PartialService<E> {
            /// Parse known fields directly so YAML errors retain their original spans.
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct ServiceVisitor<E>(PhantomData<E>);
                impl<'de, E: Extensions> Visitor<'de> for ServiceVisitor<E> {
                    type Value = PartialService<E>;
                    /// Describe the expected YAML shape in parser errors.
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("service settings") }
                    /// Delay preset/key agreement until inherited presets are resolved.
                    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                        let mut result = PartialService::default();
                        let mut seen = std::collections::HashSet::new();
                        while let Some(key) = map.next_key::<String>()? {
                            if !seen.insert(key.clone()) { return Err(serde::de::Error::custom(format!("duplicate key `{key}`"))); }
                            match key.as_str() {
                                $(stringify!($field) => result.$field = map.next_value()?,)*
                                "env" => result.env = Some(map.next_value()?),
                                "gui" => return Err(serde::de::Error::custom("`gui` is not supported yet")),
                                key if E::preset_names().contains(&key) || E::accepts_unregistered_presets() => {
                                    let value: Spanned<PresetMapping> = map.next_value()?;
                                    let parsed = E::parse_preset_options(key, serde_json::Value::Object(value.value.0)).map_err(|mut message| {
                                        let known = [$(stringify!($field),)* "env"];
                                        if let Some(suggestion) = known.iter().max_by(|a,b| strsim::jaro_winkler(key,a).total_cmp(&strsim::jaro_winkler(key,b)))
                                            && strsim::jaro_winkler(key,suggestion) > 0.75 {
                                            message.push_str(&format!("; did you mean `{suggestion}`?"));
                                        }
                                        serde::de::Error::custom(message)
                                    })?;
                                    result.preset_options.insert(key.into(), Spanned::new(parsed, value.referenced, value.defined));
                                }
                                key => {
                                    let known = [$(stringify!($field),)* "env"];
                                    let suggestion = known.iter().max_by(|a,b| strsim::jaro_winkler(key,a).total_cmp(&strsim::jaro_winkler(key,b)));
                                    let message = match suggestion.filter(|s| strsim::jaro_winkler(key,s) > 0.75) {
                                        Some(s) => format!("unknown key `{key}`; did you mean `{s}`?"),
                                        None => format!("unknown key `{key}`"),
                                    };
                                    return Err(serde::de::Error::custom(message));
                                }
                            }
                        }
                        Ok(result)
                    }
                }
                d.deserialize_map(ServiceVisitor(PhantomData))
            }
        }

        impl<'de, E: Extensions> Deserialize<'de> for ServiceConfig<E> {
            /// Deserialize a standalone service, retaining strict unknown-key checks.
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                PartialService::<E>::deserialize(d).map(PartialService::finish)
            }
        }

        impl<E: Extensions> Serialize for ServiceConfig<E> {
            /// Emit preset options under their declared names without flattening.
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut map = s.serialize_map(None)?;
                $(if let Some(value) = &self.$field { map.serialize_entry(stringify!($field), value)?; })*
                if !self.env.is_empty() { map.serialize_entry("env", &self.env)?; }
                for (key, value) in &self.preset_options { map.serialize_entry(key, value)?; }
                map.end()
            }
        }
    }
}

service_fields! {
    /// Template name; resolved after devlanes.local.yaml is merged.
    #[schemars(extend("examples" = ["fastify-api"]))]
    extends: String,
    /// Shell command or argv; exactly one of run and run_script is required after inheritance.
    run: Run,
    /// Script file and name, in shorthand or object form.
    run_script: RunScript,
    /// Working directory relative to the lane worktree.
    #[schemars(extend("examples" = ["apps/api"]))]
    cwd: Templated<String>,
    /// Platform: node, bun, deno, python or rust; detected from project files when omitted.
    #[schemars(extend("examples" = ["node","bun","deno","python","rust"]))]
    platform: String,
    /// Registered framework preset; options use a sibling key with this name.
    #[schemars(extend("examples" = ["vite","next","wrangler","tauri"]))]
    preset: String,
    /// HTTP hosts, wildcard hosts, and routes to named ports.
    #[schemars(extend("examples" = [["api.${{ lane.domain }}"],[{"host":"inspect.${{ lane.domain }}","port":"inspector"}]]))]
    hosts: Vec<HostConfig>,
    /// Tags used by +tag profile selectors.
    #[schemars(extend("examples" = [["api","essential"],["worker"],["web"]]))]
    tags: Vec<String>,
    /// Readiness policy; defaults to TCP with hosts, otherwise started.
    ready: ReadyConfig,
    /// Use a pseudoterminal; defaults to true.
    #[schemars(extend("examples" = [true,false]))]
    pty: Templated<bool>,
    /// Stop signal and grace period before killing the process tree.
    stop: StopConfig,
    /// Prevent remote port mirroring; defaults to false.
    #[schemars(extend("examples" = [false,true]))]
    hidden: Templated<bool>,
    /// Start by default unless explicitly selected; defaults to true.
    #[schemars(extend("examples" = [true,false]))]
    autostart: Templated<bool>,
    /// Override the project fnox profile for this service.
    fnox: FnoxConfig,
    /// Folders copied to stable local or remote-client paths.
    #[schemars(extend("examples" = [[{"from":"apps/extension/.output/chrome-mv3","ignore":["*.map"]}]]))]
    sync: Vec<SyncConfig>,
    /// Number of instances; replica.index starts at one. Defaults to one.
    #[schemars(extend("examples" = [1,2]))]
    replicas: Templated<u16>,
}

impl<E: Extensions> ServiceConfig<E> {
    /// Return the implicit readiness policy when no explicit check is configured.
    pub fn effective_ready(&self) -> ReadyConfig {
        self.ready.clone().unwrap_or_else(|| {
            if self.hosts.as_ref().is_some_and(|hosts| !hosts.is_empty()) {
                ReadyConfig::Check(crate::ReadyCheck {
                    tcp: Some(crate::TcpReady::Enabled(true)),
                    ..Default::default()
                })
            } else {
                ReadyConfig::Started(crate::Started::Started)
            }
        })
    }
}
