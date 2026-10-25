use crate::{
    ConfigDuration, Env, Extensions, Hosts, OpaqueExtensions, ReadyConfig, Run, RunScript,
    SyncConfig, Templated,
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
/// How devlanes stops the service on `devlanes stop`, a restart or a lane stop: it sends
/// `signal` to every process of the service, waits up to `timeout`, then SIGKILLs whatever
/// is still running. Defaults to SIGTERM and 10s.
#[schemars(extend("examples" = [{"signal":"SIGINT","timeout":"15s"}]))]
pub struct StopConfig {
    /// Signal sent to the service's whole process tree when devlanes stops or restarts it.
    /// One of `SIGTERM` (default), `SIGINT`, `SIGHUP`, `SIGQUIT`, `SIGUSR1`, `SIGUSR2` or
    /// `SIGKILL`; use `SIGINT` for programs that only shut down cleanly on Ctrl-C.
    #[schemars(extend("examples" = ["SIGTERM","SIGINT"]))]
    pub signal: Option<String>,
    /// How long to wait after `signal` before devlanes SIGKILLs every process of the service
    /// that is still running. The service then shows as stopped; devlanes never restarts it.
    /// Defaults to 10s; raise it for programs that need time to flush or close connections.
    #[schemars(extend("examples" = ["15s","10m"]))]
    pub timeout: Option<Templated<ConfigDuration>>,
}

macro_rules! service_fields {
    ($($(#[$meta:meta])* $field:ident: $ty:ty),* $(,)?) => {
        /// A long-running process started with the lane, such as a dev server or a worker.
        /// Each service gets its own port from the lane's block (`PORT`, `${{ self.port }}`);
        /// devlanes stops it with the lane and never restarts it when it exits. A service needs
        /// `run` or `run_script`, directly or through `extends`.
        #[derive(Debug, Clone, JsonSchema)]
        #[schemars(bound = "E: Extensions", deny_unknown_fields)]
        #[schemars(extend("examples" = [{"run_script":{"file":"apps/api/package.json","name":"dev"},"hosts":"api.${{ lane.domain }}","tags":["api"]}]))]
        pub struct ServiceConfig<E: Extensions = OpaqueExtensions> {
            $($(#[$meta])* pub $field: Option<$ty>,)*
            /// Environment variables for this service's process. They override `global_env` and
            /// are overridden by the active profile's `env` and `--env`; values may be text,
            /// numbers, booleans or `${{ ... }}` expressions. Setting `PORT` replaces the port
            /// devlanes allocated; a literal number is the same in every lane, so lanes collide.
            #[schemars(default)]
            #[schemars(extend("examples" = [{"API_URL":"${{ services.api.url }}"}]))]
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
            /// Apply a local override or an `extends` child. Mappings (`ready`, `stop`,
            /// `run_script`, `env`, preset options) merge per key; lists and
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
    /// Label shown in the TUI and human output instead of the service key; any text, spaces
    /// allowed, with ` #N` added per replica. Commands, profiles and `${{ services.<key> }}`
    /// keep using the key.
    #[schemars(extend("examples" = ["Admin API","Storefront (web)"]))]
    name: String,
    /// Template under `templates` whose settings this service inherits; its own settings
    /// apply on top. Mappings (`env`, `ready`, `stop`, preset options, `run_script`)
    /// merge by key, lists (`tags`, `hosts`, `sync`) and single values replace. Setting `run`
    /// or `run_script` drops the inherited other one, and one `ready` probe (`tcp`, `http`,
    /// `log`, `cmd`) drops the inherited probe. Templates may extend other templates and are
    /// resolved after devlanes.local.yaml is merged.
    #[schemars(extend("examples" = ["fastify-api"]))]
    extends: String,
    /// Command that starts the service. A string runs with `/bin/sh -c`, so pipes, `&&` and
    /// multi-line scripts work; a list is an argv run directly without a shell. It starts in
    /// the lane's worktree root unless `cwd` is set. devlanes runs it exactly as written, so
    /// it cannot be combined with `preset`; prefer `run_script` when the command is a script
    /// in a project file. Exactly one of `run` and `run_script` is required after templates
    /// are applied.
    run: Run,
    /// Script to run from a project file, as `{ file, name }`, such as
    /// `{ file: apps/api/package.json, name: dev }`. devlanes picks the tool from the file
    /// name (`package.json`, `Makefile`, `justfile`, `deno.json`, `pyproject.toml`,
    /// `Cargo.toml`), runs the script in that file's directory and checks that it exists
    /// before starting. Presets need it to place their flags, and a profile's `run_scripts`
    /// can swap the script name. Exactly one of `run` and `run_script` is required after
    /// templates are applied.
    run_script: RunScript,
    /// Directory `run` starts in, relative to the lane's worktree; defaults to the worktree
    /// root. Platform detection also looks here. Cannot be combined with `run_script`, which
    /// runs in the script file's directory; `devlanes config check` refuses the pair,
    /// including a `cwd` inherited through `extends`.
    #[schemars(extend("examples" = ["apps/api"]))]
    cwd: Templated<String>,
    /// Runtime of the service: `node`, `bun`, `deno`, `python` or `rust`. It decides which
    /// variables devlanes adds, such as the devlanes CA (`NODE_EXTRA_CA_CERTS`,
    /// `SSL_CERT_FILE`) and color settings. Detected from the `run_script` file or the working
    /// directory when omitted; set it only when detection is wrong.
    #[schemars(extend("examples" = ["node","bun","deno","python","rust"]))]
    platform: String,
    /// Framework integration that adds the flags and variables a dev server needs to run in
    /// a lane, such as `--port <port> --strictPort --host 127.0.0.1` for `vite`, so you do
    /// not pass them yourself. Requires `run_script`; with a hand-written `run`, drop the
    /// preset and pass its flags yourself. Its options go under a sibling key with the
    /// preset's name, for example `vite: { args: [--open] }`.
    #[schemars(extend("examples" = ["vite","next","wrangler","tauri"]))]
    preset: String,
    /// Hostnames the devlanes HTTPS proxy routes to this service, so `https://<host>` reaches
    /// it. One entry or a list; an entry is a hostname or a mapping with `host` and optional
    /// `port` (a named port), `fallback` and `host_header`. A `*.` wildcard matches
    /// subdomains, and an exact host wins over it. The first non-wildcard host becomes
    /// `public_url`, non-wildcard hosts join the lane's `DEVLANES_ORIGINS`, and having hosts
    /// makes the default `ready` a TCP check. Keep them under `${{ lane.domain }}` so lanes
    /// do not collide.
    #[schemars(extend("examples" = ["api.${{ lane.domain }}",["api.${{ lane.domain }}"],[{"host":"inspect.${{ lane.domain }}","port":"inspector"}]]))]
    hosts: Hosts,
    /// Labels for selecting and grouping services. `+tag` selects every service with the
    /// tag in profiles, in commands such as `devlanes start +api`, and in the TUI filter.
    /// Tags also set the service's color: the first tag with a `color` under the top-level
    /// `tags` wins, then the first tag naming a category such as `api`, `web` or `worker`,
    /// so put that tag first.
    #[schemars(extend("examples" = [["api","essential"],["worker"],["web"]]))]
    tags: Vec<String>,
    /// When the service counts as up: `devlanes start` waits for it, and `after: services`
    /// tasks run once every started service is ready. Write `started` (ready once the
    /// process runs) or a mapping with one of `tcp`, `http`, `log` or `cmd`, plus `timeout`
    /// (default 60s). Without it, a service with `hosts` waits for TCP on its port and one
    /// without hosts uses `started`. A check that times out marks the service failed, which
    /// also skips `after: services` tasks; the process keeps running and is not restarted.
    ready: ReadyConfig,
    /// Run the service in a pseudoterminal, so it sees a terminal as it would in your shell
    /// and you can type into it from the TUI. Set false to use pipes instead: stdin is empty
    /// and stdout and stderr are read separately. Defaults to true.
    #[schemars(extend("examples" = [true,false]))]
    pty: Templated<bool>,
    /// How devlanes stops the service: `signal` goes to its whole process tree, and after
    /// `timeout` every process still running is SIGKILLed. The service then shows as
    /// stopped and is not restarted. Defaults to SIGTERM and 10s.
    stop: StopConfig,
    /// Keep this service's ports and hosts off machines connected with
    /// `devlanes remote connect`: they are not mirrored there and cannot be reached from
    /// there. The service still runs and works on this machine. Defaults to false.
    #[schemars(extend("examples" = [false,true]))]
    hidden: Templated<bool>,
    /// Set false for an on-demand service: a plain `devlanes start` without a `default`
    /// profile skips it, and so do globs such as `*` in profiles. Naming it or its `+tag`,
    /// on the command line or in a profile, still starts it. An expression is evaluated for
    /// each lane and acts like the `true` or `false` it gives, such as
    /// `${{ lane.index == 0 }}` to autostart only in the main lane. With `replicas`, the
    /// service starts all its copies when any copy's expression gives true. Defaults to true.
    #[schemars(extend("examples" = [true,false]))]
    autostart: Templated<bool>,
    /// Folders mirrored while the service runs, such as build output a browser loads
    /// unpacked from a fixed path. Each entry copies `from` to `to` (default
    /// `~/devlanes/sync/<project>/<lane>/<folder>`) and follows changes; machines connected
    /// with `devlanes remote connect` get their own copy.
    #[schemars(extend("examples" = [[{"from":"apps/extension/.output/chrome-mv3","ignore":["*.map"]}]]))]
    sync: Vec<SyncConfig>,
    /// Number of copies of this service to run, each with its own port, for example several
    /// queue workers. They run as `<name>.1` to `<name>.N`, `${{ replica.index }}` counts
    /// from 1, and `services.<name>` in expressions means the first copy; use
    /// `${{ replica.index }}` in `hosts` to give each copy its own hostname. A plain number
    /// of at least 1, the same in every lane, because each copy reserves ports in every
    /// lane's block; `${{ ... }}` expressions are not allowed. Defaults to 1.
    #[schemars(extend("examples" = [1,2]))]
    replicas: u16,
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
