use crate::{ByteSize, ConfigDuration, ConfigError, HostsFile, Percent, Templated};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{num::NonZeroU32, path::Path};

/// Per-user machine settings, separate from project configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UserConfig {
    /// Hostname resolution strategy and optional hosts-file policy.
    pub hostnames: HostnamesConfig,
    /// Machine-wide pool shared by all project lane blocks.
    pub ports: PortsConfig,
    /// Certificate authority and explicitly requested trust targets.
    pub tls: TlsConfig,
    /// HTTP/HTTPS listening ports and LAN access policy.
    pub proxy: ProxyConfig,
    /// Disk retention for service output.
    pub logs: LogsConfig,
    /// Retention for monitor aggregates.
    pub metrics: MetricsConfig,
    /// How many services start at once, across all lanes and projects.
    pub startup: StartupConfig,
    /// Terminal application used by open-in-terminal actions.
    pub terminal: Option<Terminal>,
}

/// The startup mode used when `startup.mode` is absent. `fixed` ships as the
/// default until a measured comparison shows `adaptive` reaching ready sooner
/// than `all`; switching the default is a change to this constant only.
pub const DEFAULT_STARTUP_MODE: StartupMode = StartupMode::Fixed;

/// How the start gate sizes its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StartupMode {
    /// Grow the window while CPU and memory allow, shrink it under pressure.
    Adaptive,
    /// Always `max` starts at a time.
    Fixed,
    /// No pacing: every service starts at once.
    All,
}

impl Default for StartupMode {
    /// [`DEFAULT_STARTUP_MODE`].
    fn default() -> Self {
        DEFAULT_STARTUP_MODE
    }
}

/// Startup pacing. Machine settings, so they live in user config only.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StartupConfig {
    /// `adaptive`, `fixed` or `all`; default `fixed`.
    pub mode: StartupMode,
    /// Adaptive starting window; default CPU cores / 4, at least 2, at most `max`.
    pub initial: Option<NonZeroU32>,
    /// Largest window (the fixed window in `fixed` mode); default CPU cores.
    pub max: Option<NonZeroU32>,
    /// Adaptive mode grows the window while system CPU stays below this; default 75%.
    pub target_cpu: Percent,
    /// A `ready: started` service has finished starting once its process tree
    /// stays below this share of one core for 2 s; default 10%.
    pub settle_cpu: Percent,
    /// A start slot is given back after this even if the service is not ready,
    /// so services waiting on each other cannot block the queue; default 20s.
    pub slot_timeout: ConfigDuration,
}

impl Default for StartupConfig {
    /// The documented defaults; window sizes follow the CPU count at runtime.
    fn default() -> Self {
        Self {
            mode: StartupMode::default(),
            initial: None,
            max: None,
            target_cpu: Percent(75.0),
            settle_cpu: Percent(10.0),
            slot_timeout: ConfigDuration(std::time::Duration::from_secs(20)),
        }
    }
}

impl StartupConfig {
    /// Effective `(initial, max)` windows for a machine with `cores` CPUs: `max`
    /// defaults to the core count, `initial` to a quarter of it (at least 2),
    /// and `initial` never exceeds `max`.
    pub fn windows(&self, cores: usize) -> (usize, usize) {
        let cores = cores.max(1);
        let to_usize = |value: NonZeroU32| usize::try_from(value.get()).unwrap_or(usize::MAX);
        let max = self.max.map_or(cores, to_usize);
        let initial = self.initial.map_or((cores / 4).max(2), to_usize);
        (initial.min(max), max)
    }

    /// Reject ranges the parser cannot express.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.target_cpu.0 <= 0.0 || self.target_cpu.0 > 100.0 {
            return Err(ConfigError::plain(
                "startup.target_cpu must be above 0% and at most 100%",
            ));
        }
        if self.slot_timeout.0.is_zero() {
            return Err(ConfigError::plain(
                "startup.slot_timeout must be longer than 0s",
            ));
        }
        if let (Some(initial), Some(max)) = (self.initial, self.max)
            && initial > max
        {
            return Err(ConfigError::plain(
                "startup.initial must not be larger than startup.max",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HostnamesConfig {
    /// localhost or resolver in v1; public parses but is not supported yet.
    pub resolver: Resolver,
    /// Whether exact hostnames should also be written to the system hosts file.
    pub hosts_file: Option<HostsFile>,
    /// Custom top-level domain for the embedded DNS strategy.
    pub tld: Option<String>,
    /// Reserved public-resolver base domain; not supported yet.
    pub base_domain: Option<String>,
    /// Reserved DNS-01 provider settings; not supported yet.
    pub dns_provider: Option<DnsProvider>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Resolver {
    #[default]
    Localhost,
    HostsFile,
    Resolver,
    Public,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DnsProvider {
    pub cloudflare: CloudflareDns,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloudflareDns {
    pub token: Templated<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PortsConfig {
    pub range: String,
}

impl Default for PortsConfig {
    /// Keep the default pool below Linux's usual ephemeral range.
    fn default() -> Self {
        Self {
            range: "20000-32767".into(),
        }
    }
}

impl PortsConfig {
    /// Validate and return the inclusive machine-wide port pool.
    pub fn bounds(&self) -> Result<(u16, u16), ConfigError> {
        let bounds = self
            .range
            .split_once('-')
            .and_then(|(a, b)| Some((a.parse::<u16>().ok()?, b.parse::<u16>().ok()?)));
        bounds.filter(|(a, b)| *a > 0 && a <= b).ok_or_else(|| {
            ConfigError::plain(
                "ports.range must be an ascending nonzero range, such as 20000-32767",
            )
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    pub ca: CaKind,
    pub trust: Vec<TrustTarget>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaKind {
    #[default]
    Devlanes,
    Mkcert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TrustTarget {
    System,
    Firefox,
    Chrome,
    IosSimulator,
    AndroidEmulator,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyConfig {
    pub https_port: u16,
    pub http_port: u16,
    pub lan: bool,
}

impl Default for ProxyConfig {
    /// Prefer standard ports; runtime binding handles permission-related fallbacks.
    fn default() -> Self {
        Self {
            https_port: 443,
            http_port: 80,
            lan: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LogsConfig {
    pub retention: LogRetention,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LogRetention {
    pub days: u32,
    pub max_size: ByteSize,
}

impl Default for LogRetention {
    /// Bound default disk retention by age and total bytes.
    fn default() -> Self {
        Self {
            days: 7,
            max_size: ByteSize(5 << 30),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    pub retention: ConfigDuration,
}

impl Default for MetricsConfig {
    /// Keep thirty days of persisted monitor aggregates.
    fn default() -> Self {
        Self {
            retention: ConfigDuration(std::time::Duration::from_secs(30 * 86_400)),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Terminal {
    Iterm2,
    Ghostty,
    Wezterm,
    Kitty,
    Terminal,
}

impl UserConfig {
    /// Reject deferred v2 settings and invalid machine-wide resource ranges.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.hostnames.resolver == Resolver::Public
            || self.hostnames.base_domain.is_some()
            || self.hostnames.dns_provider.is_some()
        {
            return Err(ConfigError::plain(
                "public resolver and DNS-01 settings are not supported yet",
            ));
        }
        if self
            .tls
            .trust
            .iter()
            .any(|t| matches!(t, TrustTarget::IosSimulator | TrustTarget::AndroidEmulator))
        {
            return Err(ConfigError::plain(
                "iOS simulator and Android emulator certificate trust are not supported yet",
            ));
        }
        self.ports.bounds()?;
        if self.proxy.http_port == 0
            || self.proxy.https_port == 0
            || self.proxy.http_port == self.proxy.https_port
        {
            return Err(ConfigError::plain(
                "proxy ports must be nonzero and distinct",
            ));
        }
        if self.hostnames.resolver == Resolver::Resolver
            && self.hostnames.tld.as_ref().is_none_or(String::is_empty)
        {
            return Err(ConfigError::plain(
                "hostnames.tld is required for the resolver strategy",
            ));
        }
        self.startup.validate()?;
        Ok(())
    }
}

/// Parse machine settings separately from project settings and validate v1 support.
pub fn parse_user(source: &str) -> Result<UserConfig, ConfigError> {
    let config: UserConfig = crate::loader::parse_yaml("config.yaml", source)?;
    config
        .validate()
        .map_err(|e| ConfigError::at("config.yaml", source, 0, e.message))?;
    Ok(config)
}

/// Load an explicitly selected user config file; absence is an I/O error.
pub fn load_user(path: impl AsRef<Path>) -> Result<UserConfig, ConfigError> {
    let path = path.as_ref();
    let source = std::fs::read_to_string(path).map_err(|e| ConfigError::io(path, e))?;
    let config: UserConfig = crate::loader::parse_yaml(&path.display().to_string(), &source)?;
    config
        .validate()
        .map_err(|e| ConfigError::at(&path.display().to_string(), &source, 0, e.message))?;
    Ok(config)
}
