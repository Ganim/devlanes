//! `preset: wrangler`: per-lane ports, vars and dev registry for `wrangler dev`.
//!
//! Flags follow <https://developers.cloudflare.com/workers/wrangler/commands/workers/#dev>:
//! `--port`, `--inspector-port` and `--var key:value`. Vars go on the command
//! line because `vars` in the Wrangler config override inherited process
//! environment, while `--var` overrides the config.

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{ContribCx, Contributor, LaunchDraft, ServiceView},
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::collections::BTreeMap;
use std::path::Path;

/// Preset name, also the key that holds [`WranglerOptions`] in a service.
pub const NAME: &str = "wrangler";

/// Named port used for `--inspector-port`.
pub const INSPECTOR_PORT: &str = "inspector";

/// Wrangler's file-based dev registry location. Not in the user docs; defined in
/// `packages/workers-utils/src/environment-variables/misc-variables.ts` of
/// cloudflare/workers-sdk. Without it, same-named workers in two lanes resolve
/// each other's service bindings through the shared global registry.
pub const REGISTRY_ENV: &str = "WRANGLER_REGISTRY_PATH";

/// Registry directory in the lane data folder, shared by the lane's workers. Not in the
/// worktree: lanes that run from one folder would share it there.
pub const REGISTRY_DIR: &str = "wrangler-registry";

/// Script and `args` flags that collide with the ones this preset adds. yargs also
/// accepts the camelCase spelling of every dashed option.
const RESERVED_FLAGS: &[&str] = &["--port", "--inspector-port", "--inspectorPort"];

/// Options for `preset: wrangler`, which gives each lane's `wrangler dev` its own
/// `--port <self.port>`, `--inspector-port <self.ports.inspector>` and dev registry. The
/// `inspector` port is allocated automatically. `args` that pass either port flag are refused
/// by `devlanes config check`, and a `run_script` script that does is refused at start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"vars":{"PUBLIC_BASE_URL":"${{ self.public_url }}"},"args":["--env-file",".dev.vars"],"isolate_registry":true}]))]
pub struct WranglerOptions {
    /// Worker variables for this lane, each passed as `--var KEY:VALUE` so it overrides the
    /// same name in the Wrangler config's `vars`. Values can be strings, numbers or booleans
    /// (sent as text) and may use expressions such as `${{ self.public_url }}`; names must be
    /// non-empty and cannot contain `:`. Local overrides and `extends` children merge with
    /// inherited vars per name.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "scalar_map"
    )]
    #[schemars(schema_with = "scalar_map_schema", extend("examples" = [{"PUBLIC_BASE_URL":"${{ self.public_url }}"}]))]
    pub vars: BTreeMap<String, String>,
    /// Extra arguments for `wrangler dev`, added after the port and `--var` flags, for example
    /// `[--env-file, .dev.vars]`. `--port` and `--inspector-port` (or `--inspectorPort`) are
    /// refused here, since the preset already sets them per lane. A local override or
    /// `extends` child that sets `args` replaces the inherited list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("examples" = [["--env-file",".dev.vars"]]))]
    pub args: Option<Vec<String>>,
    /// Points `WRANGLER_REGISTRY_PATH` at `wrangler-registry` in the lane's data folder
    /// (`${{ lane.data_path }}`), so service bindings resolve to workers of the same lane
    /// instead of same-named workers in another lane, also when two lanes run from one
    /// folder. Set `false` to drop the variable for this service, which then uses
    /// Wrangler's shared default registry. Default: `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("examples" = [true,false]))]
    pub isolate_registry: Option<bool>,
}

/// Reads `vars` values as strings, numbers or booleans.
fn scalar_map<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let raw = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
    raw.into_iter()
        .map(|(key, value)| match value {
            serde_json::Value::String(text) => Ok((key, text)),
            serde_json::Value::Number(number) => Ok((key, number.to_string())),
            serde_json::Value::Bool(flag) => Ok((key, flag.to_string())),
            other => Err(D::Error::custom(format!(
                "wrangler var `{key}` must be a string, number or boolean, not {other}"
            ))),
        })
        .collect()
}

/// Schema matching [`scalar_map`].
fn scalar_map_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": { "type": ["string", "number", "boolean"] }
    })
}

impl WranglerOptions {
    /// Applies a local override or `extends` overlay: vars merge per key, set
    /// fields replace.
    pub fn merge(&mut self, overlay: Self) {
        self.vars.extend(overlay.vars);
        if overlay.args.is_some() {
            self.args = overlay.args;
        }
        if overlay.isolate_registry.is_some() {
            self.isolate_registry = overlay.isolate_registry;
        }
    }
}

/// Arguments and environment a preset adds to a service's launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contribution {
    /// Appended to the command; runners decide whether a `--` precedes them.
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Everything [`contribution`] needs for one service in one lane.
#[derive(Debug, Clone, Copy)]
pub struct WranglerInput<'a> {
    /// `self.port`.
    pub port: u16,
    /// `self.ports.<name>`; must contain [`INSPECTOR_PORT`].
    pub ports: &'a BTreeMap<String, u16>,
    /// The lane data folder (`lane.data_path`); the registry goes below it.
    pub lane_data: &'a Path,
    /// Options after expression evaluation.
    pub options: &'a WranglerOptions,
    /// Body of the `run_script` script, when the service uses one.
    pub script_body: Option<&'a str>,
}

/// Errors that stop a wrangler service from starting.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WranglerError {
    #[error("preset wrangler needs the named port `{name}`, which was not allocated")]
    MissingPort { name: &'static str },
    #[error("wrangler var `{key}` is invalid: {reason}")]
    InvalidVar { key: String, reason: &'static str },
    #[error(
        "the script hardcodes {}, which preset wrangler sets per lane. Remove {} from the script; devlanes passes `--port <self.port> --inspector-port <self.ports.inspector>`. To keep the flags, drop `preset: wrangler` and use `run` with `--port ${{{{ self.port }}}} --inspector-port ${{{{ self.ports.inspector }}}}`",
        .flags.join(", "),
        if .flags.len() == 1 { "it" } else { "them" }
    )]
    HardcodedFlags { flags: Vec<String> },
    #[error(
        "`args` passes {}, which preset wrangler sets per lane. Remove {} from `args`; devlanes passes `--port <self.port> --inspector-port <self.ports.inspector>`. To keep the flags, drop `preset: wrangler` and use `run` with `--port ${{{{ self.port }}}} --inspector-port ${{{{ self.ports.inspector }}}}`",
        .flags.join(", "),
        if .flags.len() == 1 { "it" } else { "them" }
    )]
    ArgsFlags { flags: Vec<String> },
}

/// Returns the named ports to allocate before expressions are evaluated, so
/// `self.ports.inspector` exists for routes and readiness checks too.
pub fn required_ports() -> &'static [&'static str] {
    &[INSPECTOR_PORT]
}

/// Rejects a script that already passes `--port` or `--inspector-port`.
///
/// Matching is token-based and errs towards reporting: a reserved flag
/// anywhere in the body counts, including inside a wrapper command, because
/// appending a second `--port` would either be ignored or turn into an array
/// that wrangler rejects.
pub fn check_script(body: &str) -> Result<(), WranglerError> {
    let tokens: Vec<&str> = body
        .split_whitespace()
        .map(|token| token.trim_matches(|c| c == '"' || c == '\''))
        .collect();
    let flags = reserved_flags(&tokens);
    if flags.is_empty() {
        Ok(())
    } else {
        Err(WranglerError::HardcodedFlags { flags })
    }
}

/// Rejects `args` that pass `--port` or `--inspector-port`, which the preset
/// adds itself.
pub fn check_args(args: &[String]) -> Result<(), WranglerError> {
    let tokens: Vec<&str> = args.iter().map(String::as_str).collect();
    let flags = reserved_flags(&tokens);
    if flags.is_empty() {
        Ok(())
    } else {
        Err(WranglerError::ArgsFlags { flags })
    }
}

/// Renders each reserved flag in `tokens` with its value, such as `` `--port 8787` ``.
fn reserved_flags(tokens: &[&str]) -> Vec<String> {
    let mut flags = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let (flag, inline_value) = token
            .split_once('=')
            .map_or((*token, None), |(f, v)| (f, Some(v)));
        if !RESERVED_FLAGS.contains(&flag) {
            continue;
        }
        let rendered = match (inline_value, tokens.get(index + 1)) {
            (Some(_), _) => (*token).to_owned(),
            (None, Some(next)) if !next.starts_with('-') => format!("{flag} {next}"),
            (None, _) => flag.to_owned(),
        };
        flags.push(format!("`{rendered}`"));
    }
    flags
}

/// Renders `--port`, `--inspector-port`, `--var` and user args, plus the
/// per-lane registry variable.
pub fn contribution(input: &WranglerInput<'_>) -> Result<Contribution, WranglerError> {
    if let Some(body) = input.script_body {
        check_script(body)?;
    }
    check_args(input.options.args.as_deref().unwrap_or_default())?;
    let inspector = input
        .ports
        .get(INSPECTOR_PORT)
        .ok_or(WranglerError::MissingPort {
            name: INSPECTOR_PORT,
        })?;
    let mut args = vec![
        "--port".to_owned(),
        input.port.to_string(),
        "--inspector-port".to_owned(),
        inspector.to_string(),
    ];
    for (key, value) in &input.options.vars {
        validate_var(key)?;
        // Wrangler splits on the first `:`, so values may contain more (URLs).
        args.push("--var".to_owned());
        args.push(format!("{key}:{value}"));
    }
    args.extend(input.options.args.iter().flatten().cloned());
    let mut env = BTreeMap::new();
    if input.options.isolate_registry != Some(false) {
        let registry = input.lane_data.join(REGISTRY_DIR);
        env.insert(
            REGISTRY_ENV.to_owned(),
            registry.to_string_lossy().into_owned(),
        );
    }
    Ok(Contribution { args, env })
}

/// Rejects keys that wrangler would split or drop.
fn validate_var(key: &str) -> Result<(), WranglerError> {
    let reason = if key.is_empty() {
        "the name is empty"
    } else if key.contains(':') {
        "the name contains `:`, which wrangler uses as the name/value separator"
    } else {
        return Ok(());
    };
    Err(WranglerError::InvalidVar {
        key: key.to_owned(),
        reason,
    })
}

/// JSON Schema of [`WranglerOptions`], for the config's `Extensions` registration.
pub fn options_schema() -> schemars::Schema {
    schemars::schema_for!(WranglerOptions)
}

/// Parses evaluated options; a service without a `wrangler:` key gets defaults.
/// Port flags in `args` are refused here so `devlanes config check` reports them.
pub fn parse_options(value: Option<&serde_json::Value>) -> Result<WranglerOptions, Error> {
    let options: WranglerOptions = match value {
        None | Some(serde_json::Value::Null) => return Ok(WranglerOptions::default()),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| Error::InvalidInput(format!("{NAME} options: {e}")))?,
    };
    check_args(options.args.as_deref().unwrap_or_default())
        .map_err(|e| Error::InvalidInput(format!("{NAME} options: {e}")))?;
    Ok(options)
}

/// Core contributor for services with `preset: wrangler`.
#[derive(Debug, Default, Clone, Copy)]
pub struct WranglerPreset;

#[async_trait]
impl Contributor for WranglerPreset {
    fn id(&self) -> &'static str {
        "preset-wrangler"
    }

    fn priority(&self) -> i32 {
        100
    }

    /// Asks for `inspector` on wrangler services only.
    fn required_ports(&self, service: &ServiceView<'_>) -> Vec<String> {
        if service.preset == Some(NAME) {
            required_ports()
                .iter()
                .map(|name| (*name).to_owned())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Appends ports, vars and args. With `run_script`, the script body from
    /// the context is checked for hardcoded port flags first.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        if cx.preset != Some(NAME) {
            return Ok(());
        }
        let options = parse_options(cx.preset_options)?;
        let port = cx
            .port
            .ok_or_else(|| Error::InvalidInput(format!("preset {NAME} needs a service port")))?;
        let contribution = contribution(&WranglerInput {
            port,
            ports: cx.ports,
            lane_data: cx.lane_data_path,
            options: &options,
            script_body: cx.script.and_then(|script| script.body.as_deref()),
        })
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
        // Presets require `run_script`; its runner places the arguments for the tool.
        draft.args.extend(contribution.args);
        if options.isolate_registry == Some(false) {
            // The cloudflare platform contributor (priority 60) may already
            // have set the lane registry; opting out must undo that too.
            draft.env.remove(REGISTRY_ENV);
        }
        draft.env.extend(contribution.env);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports() -> BTreeMap<String, u16> {
        BTreeMap::from([(INSPECTOR_PORT.to_owned(), 20011)])
    }

    /// Flags, vars (sorted by key) and user args render in a fixed order.
    #[test]
    fn renders_ports_vars_and_args() {
        let options = WranglerOptions {
            vars: BTreeMap::from([
                (
                    "PUBLIC_BASE_URL".to_owned(),
                    "https://api.acme-1.localhost".to_owned(),
                ),
                ("A".to_owned(), "1".to_owned()),
            ]),
            args: Some(vec!["--env-file".into(), ".dev.vars.stripe".into()]),
            isolate_registry: None,
        };
        let ports = ports();
        let result = contribution(&WranglerInput {
            port: 20010,
            ports: &ports,
            lane_data: Path::new("/data/acme/7"),
            options: &options,
            script_body: Some("wrangler dev --persist-to .wrangler-state"),
        })
        .expect("valid input");
        insta::assert_snapshot!(result.args.join(" "), @"--port 20010 --inspector-port 20011 --var A:1 --var PUBLIC_BASE_URL:https://api.acme-1.localhost --env-file .dev.vars.stripe");
        let registry = Path::new("/data/acme/7").join(REGISTRY_DIR);
        assert_eq!(
            result.env.get(REGISTRY_ENV).map(String::as_str),
            Some(registry.display().to_string().as_str())
        );
        #[cfg(unix)]
        assert_eq!(
            result.env.get(REGISTRY_ENV).map(String::as_str),
            Some("/data/acme/7/wrangler-registry")
        );
    }

    /// Registry isolation can be turned off for setups that share one on purpose.
    #[test]
    fn registry_isolation_is_optional() {
        let options = WranglerOptions {
            isolate_registry: Some(false),
            ..WranglerOptions::default()
        };
        let ports = ports();
        let result = contribution(&WranglerInput {
            port: 1,
            ports: &ports,
            lane_data: Path::new("/d"),
            options: &options,
            script_body: None,
        })
        .expect("valid input");
        assert!(result.env.is_empty());
    }

    /// A missing inspector port is reported instead of rendering a bad flag.
    #[test]
    fn missing_inspector_port_is_an_error() {
        let ports = BTreeMap::new();
        let error = contribution(&WranglerInput {
            port: 1,
            ports: &ports,
            lane_data: Path::new("/d"),
            options: &WranglerOptions::default(),
            script_body: None,
        });
        assert_eq!(
            error,
            Err(WranglerError::MissingPort {
                name: INSPECTOR_PORT
            })
        );
    }

    /// Keys with `:` would be split differently by wrangler.
    #[test]
    fn rejects_var_names_with_separator() {
        let options = WranglerOptions {
            vars: BTreeMap::from([("A:B".to_owned(), "1".to_owned())]),
            ..WranglerOptions::default()
        };
        let ports = ports();
        let error = contribution(&WranglerInput {
            port: 1,
            ports: &ports,
            lane_data: Path::new("/d"),
            options: &options,
            script_body: None,
        });
        assert!(matches!(error, Err(WranglerError::InvalidVar { .. })));
    }

    /// Table of script bodies and the flags reported for them.
    #[test]
    fn hardcoded_flag_detection() {
        let cases: &[(&str, &[&str])] = &[
            ("wrangler dev", &[]),
            ("wrangler dev --persist-to .state --env-file .dev.vars", &[]),
            ("wrangler dev --port 8787", &["`--port 8787`"]),
            ("wrangler dev --port=8787", &["`--port=8787`"]),
            (
                "wrangler dev --inspector-port 9229",
                &["`--inspector-port 9229`"],
            ),
            (
                "wrangler dev --inspectorPort=9229",
                &["`--inspectorPort=9229`"],
            ),
            (
                "wrangler dev --port 8787 --inspector-port 9229",
                &["`--port 8787`", "`--inspector-port 9229`"],
            ),
            (
                "concurrently \"wrangler dev --port 8787\" \"tsc -w\"",
                &["`--port 8787`"],
            ),
            ("wrangler dev --port --local", &["`--port`"]),
            ("vite --portal 1 --report-port 2", &[]),
        ];
        for (body, expected) in cases {
            let found = match check_script(body) {
                Ok(()) => Vec::new(),
                Err(WranglerError::HardcodedFlags { flags }) => flags,
                Err(other) => panic!("unexpected error for {body:?}: {other}"),
            };
            assert_eq!(&found, expected, "body: {body}");
        }
    }

    /// The message names the flag and both ways out.
    #[test]
    fn hardcoded_flag_message_has_the_fix() {
        let error = check_script("wrangler dev --port 8787").expect_err("hardcoded port");
        insta::assert_snapshot!(error.to_string(), @"the script hardcodes `--port 8787`, which preset wrangler sets per lane. Remove it from the script; devlanes passes `--port <self.port> --inspector-port <self.ports.inspector>`. To keep the flags, drop `preset: wrangler` and use `run` with `--port ${{ self.port }} --inspector-port ${{ self.ports.inspector }}`");
    }

    /// Table of `args` lists and the flags reported for them.
    #[test]
    fn args_flag_detection() {
        let cases: &[(&[&str], &[&str])] = &[
            (&[], &[]),
            (&["--env-file", ".dev.vars", "--local"], &[]),
            (&["--port", "8787"], &["`--port 8787`"]),
            (&["--port=8787"], &["`--port=8787`"]),
            (&["--inspector-port", "9229"], &["`--inspector-port 9229`"]),
            (&["--inspectorPort=9229"], &["`--inspectorPort=9229`"]),
            (
                &["--port", "8787", "--inspectorPort", "9229"],
                &["`--port 8787`", "`--inspectorPort 9229`"],
            ),
            (&["--port", "--local"], &["`--port`"]),
            (&["--portal", "1", "--report-port", "2"], &[]),
        ];
        for (args, expected) in cases {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
            let found = match check_args(&args) {
                Ok(()) => Vec::new(),
                Err(WranglerError::ArgsFlags { flags }) => flags,
                Err(other) => panic!("unexpected error for {args:?}: {other}"),
            };
            assert_eq!(&found, expected, "args: {args:?}");
        }
    }

    /// The `args` message names the flag and both ways out, like the script one.
    #[test]
    fn args_flag_message_has_the_fix() {
        let error =
            check_args(&["--inspector-port".into(), "9229".into()]).expect_err("port flag in args");
        insta::assert_snapshot!(error.to_string(), @"`args` passes `--inspector-port 9229`, which preset wrangler sets per lane. Remove it from `args`; devlanes passes `--port <self.port> --inspector-port <self.ports.inspector>`. To keep the flags, drop `preset: wrangler` and use `run` with `--port ${{ self.port }} --inspector-port ${{ self.ports.inspector }}`");
    }

    /// Port flags in `args` fail both parsing and the contribution.
    #[test]
    fn args_flags_are_refused() {
        let value = serde_json::json!({ "args": ["--port", "8787"] });
        let error = parse_options(Some(&value)).expect_err("port flag in args");
        assert!(
            matches!(&error, Error::InvalidInput(message) if message.starts_with("wrangler options: `args` passes `--port 8787`")),
            "{error}"
        );
        let options = WranglerOptions {
            args: Some(vec!["--port=8787".into()]),
            ..WranglerOptions::default()
        };
        let ports = ports();
        let error = contribution(&WranglerInput {
            port: 1,
            ports: &ports,
            lane_data: Path::new("/d"),
            options: &options,
            script_body: None,
        });
        assert_eq!(
            error,
            Err(WranglerError::ArgsFlags {
                flags: vec!["`--port=8787`".to_owned()]
            })
        );
    }

    /// Hardcoded flags fail the contribution when a script body is supplied.
    #[test]
    fn contribution_checks_script_body() {
        let ports = ports();
        let error = contribution(&WranglerInput {
            port: 1,
            ports: &ports,
            lane_data: Path::new("/d"),
            options: &WranglerOptions::default(),
            script_body: Some("wrangler dev --inspector-port 9229"),
        });
        assert!(matches!(error, Err(WranglerError::HardcodedFlags { .. })));
    }

    /// Overlay vars merge per key; unset fields stay.
    #[test]
    fn merge_combines_vars() {
        let mut base = WranglerOptions {
            vars: BTreeMap::from([("A".to_owned(), "1".to_owned())]),
            args: Some(vec!["--x".into()]),
            isolate_registry: None,
        };
        base.merge(WranglerOptions {
            vars: BTreeMap::from([("B".to_owned(), "2".to_owned())]),
            ..WranglerOptions::default()
        });
        assert_eq!(base.vars.len(), 2);
        assert_eq!(base.args.as_deref(), Some(&["--x".to_owned()][..]));
    }
}
