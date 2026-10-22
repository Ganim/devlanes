use crate::{
    ConfigDuration, Env, Extensions, FnoxConfig, HostConfig, OpaqueExtensions, ReadyConfig, Run,
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
        pub struct ServiceConfig<E: Extensions = OpaqueExtensions> {
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
            // An option key cannot be classified until the preset has been inherited.
            pub preset_candidates: IndexMap<String, Spanned<serde_json::Value>>,
            extensions: PhantomData<E>,
        }

        impl<E: Extensions> Default for PartialService<E> {
            /// Keep every field absent until base and local sources have been merged.
            fn default() -> Self {
                Self { $($field: None,)* env: None, preset_candidates: IndexMap::new(), extensions: PhantomData }
            }
        }

        impl<E: Extensions> Default for ServiceConfig<E> {
            /// Construct an empty template; runnable services require a command.
            fn default() -> Self { Self { $($field: None,)* env: Env::new(), preset_options: IndexMap::new() } }
        }

        impl<E: Extensions> PartialService<E> {
            /// Apply a local override or an `extends` child. Mappings (`ready`, `stop`, `fnox`,
            /// `{ source, name }` scripts, `env`, preset options) merge per key; lists and
            /// scalars replace. `run` and `run_script` are alternatives, so setting one clears
            /// the other. Merged values keep the overlay's source position for diagnostics.
            pub(crate) fn merge(&mut self, mut overlay: Self) {
                if overlay.run.is_some() { self.run_script = None; }
                if overlay.run_script.is_some() { self.run = None; }
                if let (Some(base), Some(local)) = (&self.run_script, &mut overlay.run_script) {
                    let mut merged = base.value.clone();
                    crate::loader::merge_script(&mut merged, local.value.clone());
                    local.value = merged;
                }
                if let (Some(base), Some(local)) = (&self.ready, &mut overlay.ready) {
                    let mut merged = base.value.clone();
                    crate::loader::merge_ready(&mut merged, local.value.clone());
                    local.value = merged;
                }
                if let (Some(base), Some(local)) = (&self.stop, &mut overlay.stop) {
                    if local.value.signal.is_none() { local.value.signal = base.value.signal.clone(); }
                    if local.value.timeout.is_none() { local.value.timeout = base.value.timeout.clone(); }
                }
                if let (Some(base), Some(local)) = (&self.fnox, &mut overlay.fnox) {
                    if local.value.profile.is_none() { local.value.profile = base.value.profile.clone(); }
                }
                $(if let Some(value) = overlay.$field { self.$field = Some(value); })*
                if let Some(overlay) = overlay.env {
                    if let Some(base) = &mut self.env { base.value.extend(overlay.value); }
                    else { self.env = Some(overlay); }
                }
                for (key, mut value) in overlay.preset_candidates {
                    if let Some(base) = self.preset_candidates.get(&key) {
                        let mut merged = base.value.clone();
                        crate::loader::merge_json(&mut merged, value.value);
                        value.value = merged;
                    }
                    self.preset_candidates.insert(key, value);
                }
            }

            /// Classify unknown keys against the effective preset only after merging and inheritance.
            pub(crate) fn finish(self, path: &str) -> Result<ServiceConfig<E>, crate::ConfigError> {
                let mut preset_options = IndexMap::new();
                let preset = self.preset.as_ref().map(|value| value.value.as_str());
                for (key, value) in self.preset_candidates {
                    let fail = |message| crate::ConfigError::plain(format!("{path}.{key}: {message}"));
                    if preset != Some(key.as_str()) {
                        return Err(fail(unknown_service_key::<E>(&key, preset, value.value.is_object())));
                    }
                    if !value.value.is_object() {
                        return Err(fail(format!("preset options `{key}` must be a mapping")));
                    }
                    let parsed = E::parse_preset_options(&key, value.value).map_err(fail)?;
                    preset_options.insert(key, parsed);
                }
                Ok(ServiceConfig {
                    $($field: self.$field.map(|v| v.value),)*
                    env: self.env.map(|v| v.value).unwrap_or_default(),
                    preset_options,
                })
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
                                key => {
                                    result.preset_candidates.insert(key.into(), map.next_value()?);
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
                PartialService::<E>::deserialize(d)?.finish("service").map_err(serde::de::Error::custom)
            }
        }

        /// Suggest service fields or the selected integration without interpreting unrelated keys
        /// as options. Only a mapping can be preset options; without a registry every mapping key
        /// may name a preset, with one only registered names do.
        pub(crate) fn unknown_service_key<E: Extensions>(key: &str, preset: Option<&str>, mapping: bool) -> String {
            if key == "on" {
                return crate::loader::ON_HINT.into();
            }
            let fields = [$(stringify!($field),)* "env"];
            let suggestion = fields.into_iter().chain(preset).chain(E::preset_names().iter().copied())
                .filter(|candidate| *candidate != key)
                .max_by(|a,b| strsim::jaro_winkler(key,a).total_cmp(&strsim::jaro_winkler(key,b)))
                .filter(|candidate| strsim::jaro_winkler(key,candidate) > 0.75);
            if let Some(suggestion) = suggestion {
                return format!("unknown key `{key}`; did you mean `{suggestion}`?");
            }
            if preset.is_none() && mapping && (E::accepts_unregistered_presets() || E::preset_names().contains(&key)) {
                return format!("unknown key `{key}`; set `preset: {key}` to use these options");
            }
            match preset.filter(|_| mapping) {
                Some(preset) => format!("unknown key `{key}`; options must use the selected preset key `{preset}`"),
                None => format!("unknown key `{key}`"),
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
    /// Display name shown in the TUI and human output instead of the service key;
    /// any text, spaces allowed. Commands, references and `svc.<key>` keep using the key.
    #[schemars(extend("examples" = ["Admin API","Storefront (web)"]))]
    name: String,
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
