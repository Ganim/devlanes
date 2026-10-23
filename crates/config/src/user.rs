use crate::{ByteSize, ConfigDuration, ConfigError, HostsFile, Percent, Templated};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{num::NonZeroU32, path::Path, time::Duration};

/// Finished unit/task history lifetime, duplicated from the state crate to avoid a dependency.
/// The binary's retention-default test keeps these independent definitions aligned.
pub const DEFAULT_RUN_HISTORY_RETENTION: Duration = Duration::from_secs(7 * 86_400);
/// Monitor history lifetime, duplicated from the state crate and checked by the binary test.
pub const DEFAULT_METRICS_RETENTION: Duration = Duration::from_secs(7 * 86_400);
/// Disconnected remote-host lifetime, duplicated from the state crate and checked by the binary test.
pub const DEFAULT_REMOTE_HOST_RETENTION: Duration = Duration::from_secs(90 * 86_400);
/// Database and SQLite sidecar budget, duplicated from the state crate and checked by the binary test.
pub const DEFAULT_MAX_SIZE_BYTES: u64 = 512 * 1024 * 1024;

/// Per-user machine settings, separate from project configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"hostnames":{"resolver":"localhost"},"ports":{"range":"20000-32767"},"terminal":{"progress":"auto"}}]))]
pub struct UserConfig {
    /// Settings that loading ignored, reported by [`UserConfig::warnings`].
    #[serde(skip)]
    #[schemars(skip)]
    warnings: Vec<crate::ConfigWarning>,
    /// Hostname resolution strategy and optional hosts-file policy.
    pub hostnames: HostnamesConfig,
    /// Machine-wide pool shared by all project lane blocks.
    pub ports: PortsConfig,
    /// Certificate authority and explicitly requested trust targets.
    pub tls: TlsConfig,
    /// HTTP/HTTPS listening ports and LAN access policy.
    pub proxy: ProxyConfig,
    /// Retention for service output files and their database metadata.
    pub logs: LogsConfig,
    /// Retention for monitor history, including aggregates stored in the database.
    pub metrics: MetricsConfig,
    /// Retention for finished unit/task history and disconnected remote hosts in the database.
    pub history: HistoryConfig,
    /// Soft disk budget for the state database and its SQLite sidecar files.
    pub state: StateConfig,
    /// How many services start at once, across all lanes and projects.
    pub startup: StartupConfig,
    /// How devlanes uses the terminal tab's progress bar during starts.
    pub terminal: Option<TerminalConfig>,
    /// Whether the daemon looks for new devlanes releases.
    pub updates: UpdatesConfig,
    /// How commands use the terminal UI.
    pub tui: TuiConfig,
}

/// Terminal UI behaviour of the CLI.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"open_on_start":false},{"theme":"light","glyphs":"ascii"},{"on_quit":"keep"}]))]
pub struct TuiConfig {
    /// Run `start`, `restart`, `lane resume` and `lane new --start` inside the TUI when typed at a terminal; defaults to true. false prints them as text, like `--no-ui` or DEVLANES_NO_UI=1.
    #[schemars(extend("examples" = [true,false]))]
    pub open_on_start: bool,
    /// Colors of the TUI; defaults to auto. NO_COLOR always turns colors off.
    pub theme: TuiTheme,
    /// Symbols the TUI draws states and markers with; defaults to auto.
    pub glyphs: TuiGlyphs,
    /// What quitting the TUI does while lanes of the project on screen run; defaults to ask. The quit dialog's "remember" box writes it.
    pub on_quit: TuiOnQuit,
}

impl Default for TuiConfig {
    /// Starts open the TUI unless turned off.
    fn default() -> Self {
        Self {
            open_on_start: true,
            theme: TuiTheme::Auto,
            glyphs: TuiGlyphs::Auto,
            on_quit: TuiOnQuit::Ask,
        }
    }
}

/// What quitting the TUI does with running lanes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(extend("examples" = ["ask","keep","stop"]))]
pub enum TuiOnQuit {
    /// Ask each time whether to keep the lanes running or stop them.
    #[default]
    #[schemars(extend("examples" = ["ask"]))]
    Ask,
    /// Close the TUI and leave every lane running in the daemon.
    #[schemars(extend("examples" = ["keep"]))]
    Keep,
    /// Stop the running lanes of the project on screen, then close the TUI.
    #[schemars(extend("examples" = ["stop"]))]
    Stop,
}

impl TuiOnQuit {
    /// The word the TUI reads and writes: `ask`, `keep` or `stop`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Keep => "keep",
            Self::Stop => "stop",
        }
    }
}

/// Colors of the TUI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(extend("examples" = ["auto","dark","light","ansi"]))]
pub enum TuiTheme {
    /// The terminal's 16 colors, or the dark or light palette when the terminal reports true color and its background.
    #[default]
    #[schemars(extend("examples" = ["auto"]))]
    Auto,
    /// The true-color palette for a dark background.
    #[schemars(extend("examples" = ["dark"]))]
    Dark,
    /// The true-color palette for a light background.
    #[schemars(extend("examples" = ["light"]))]
    Light,
    /// The terminal's own 16 colors, which follow its theme.
    #[schemars(extend("examples" = ["ansi"]))]
    Ansi,
}

impl TuiTheme {
    /// The word the TUI reads: `auto`, `dark`, `light` or `ansi`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Ansi => "ansi",
        }
    }
}

/// Symbols the TUI draws with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(extend("examples" = ["auto","unicode","ascii"]))]
pub enum TuiGlyphs {
    /// Unicode when the locale is UTF-8, else ASCII.
    #[default]
    #[schemars(extend("examples" = ["auto"]))]
    Auto,
    /// Unicode symbols such as ● ✕ ▾.
    #[schemars(extend("examples" = ["unicode"]))]
    Unicode,
    /// Plain ASCII, for fonts or terminals that draw Unicode symbols badly.
    #[schemars(extend("examples" = ["ascii"]))]
    Ascii,
}

impl TuiGlyphs {
    /// The word the TUI reads: `auto`, `unicode` or `ascii`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Unicode => "unicode",
            Self::Ascii => "ascii",
        }
    }
}

/// Release checks: the daemon reads the latest release's version once a day and
/// commands print a one-line notice. Nothing is downloaded or installed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"check":false}]))]
pub struct UpdatesConfig {
    /// Check GitHub once a day for a newer devlanes release and show the upgrade command; defaults to true.
    #[schemars(extend("examples" = [true,false]))]
    pub check: bool,
}

impl Default for UpdatesConfig {
    /// Checks are on unless turned off.
    fn default() -> Self {
        Self { check: true }
    }
}

/// Age limits for finished unit/task history and disconnected remote-host records.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"retention":"7d","remote_hosts_retention":"90d"}]))]
pub struct HistoryConfig {
    /// Maximum age since a unit or task finished; defaults to 7d. Disk pressure may prune it sooner.
    #[schemars(extend("examples" = ["7d","30d"]))]
    pub retention: ConfigDuration,
    /// Maximum age since a remote host last connected successfully; defaults to 90d. Records without a timestamp are retained.
    #[schemars(extend("examples" = ["90d","180d"]))]
    pub remote_hosts_retention: ConfigDuration,
}

impl Default for HistoryConfig {
    /// Use the documented history defaults shared with the state retention policy.
    fn default() -> Self {
        Self {
            retention: ConfigDuration(DEFAULT_RUN_HISTORY_RETENTION),
            remote_hosts_retention: ConfigDuration(DEFAULT_REMOTE_HOST_RETENTION),
        }
    }
}

/// Soft disk budget for the database; live registry and completion records are preserved.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"max_size":"512MiB"}]))]
pub struct StateConfig {
    /// Budget for the database plus WAL and shared-memory files; defaults to 512MiB and must be nonzero. Old metrics and history are pruned first; protected live state may exceed it.
    #[schemars(extend("examples" = ["512MiB","1GiB"]))]
    pub max_size: ByteSize,
}

impl Default for StateConfig {
    /// Use the documented database budget shared with the state retention policy.
    fn default() -> Self {
        Self {
            max_size: ByteSize(DEFAULT_MAX_SIZE_BYTES),
        }
    }
}

/// The startup mode used when `startup.mode` is absent. `adaptive` starts each
/// burst at `max` like `fixed` and only slows down under CPU or memory pressure,
/// so on an idle machine it is as fast as `fixed`.
pub const DEFAULT_STARTUP_MODE: StartupMode = StartupMode::Adaptive;

/// How the start gate sizes its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(extend("examples" = ["adaptive","fixed","all"]))]
pub enum StartupMode {
    /// Begin each burst of starts at `initial` (by default `max`, full speed). Halve the
    /// slots after 3 s above 90% CPU or when the system reports memory pressure, start
    /// nothing while that pressure is critical, and add a slot every 2 s while system CPU
    /// stays below `target_cpu`, up to `max`.
    #[schemars(extend("examples" = ["adaptive"]))]
    Adaptive,
    /// Always `max` starts at a time.
    #[schemars(extend("examples" = ["fixed"]))]
    Fixed,
    /// No pacing: every service starts at once.
    #[schemars(extend("examples" = ["all"]))]
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
#[schemars(extend("examples" = [{"mode":"fixed","max":4},{"mode":"adaptive","initial":2,"max":8,"target_cpu":"75%","settle_cpu":"10%","slot_timeout":"20s","reserve_memory":"2GiB"}]))]
pub struct StartupConfig {
    /// `adaptive`, `fixed` or `all`; default `adaptive`. A starting service holds its
    /// slot until its ready check (`tcp`, `http`, `log` or `cmd`) passes, it fails, or
    /// `slot_timeout` passes; one without a check (`ready: started`) holds it until it
    /// settles (`settle_cpu`).
    pub mode: StartupMode,
    /// Adaptive starting window; defaults to `max`, so starts run at full speed until
    /// the machine is under pressure. Set it lower to ramp up from a few slots instead;
    /// never above `max`.
    #[schemars(extend("examples" = [2]))]
    pub initial: Option<NonZeroU32>,
    /// Largest window (the fixed window in `fixed` mode); default CPU cores.
    #[schemars(extend("examples" = [8]))]
    pub max: Option<NonZeroU32>,
    /// Adaptive mode grows the window while system CPU stays below this; default 75%.
    #[schemars(extend("examples" = ["75%", "80%" ]))]
    pub target_cpu: Percent,
    /// Only for services without a ready check (`ready: started`, the default for a
    /// service without `hosts`): such a service gives its slot back once its process tree
    /// uses less than this share of one core for 2 s, meaning it has finished starting up.
    /// Services with a `tcp`, `http`, `log` or `cmd` check keep their slot until the check
    /// passes instead. Defaults to `10%`.
    #[schemars(extend("examples" = ["10%","12.5%" ]))]
    pub settle_cpu: Percent,
    /// A start slot is given back after this even if the service is not ready,
    /// so services waiting on each other cannot block the queue; default 20s.
    pub slot_timeout: ConfigDuration,
    /// Free memory every new start must leave, on top of the memory the services already
    /// starting are expected to still need (each one's past startup peak, or 512 MiB
    /// without history). Defaults to 10% of RAM, at most `2GiB`; `0B` turns it off. A
    /// start with nothing else starting never waits for it. `all` ignores it.
    #[schemars(extend("examples" = ["2GiB","0B"]))]
    pub reserve_memory: Option<ByteSize>,
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
            reserve_memory: None,
        }
    }
}

impl StartupConfig {
    /// Effective `(initial, max)` windows for a machine with `cores` CPUs: `max`
    /// defaults to the core count, `initial` to `max`, and `initial` never
    /// exceeds `max`.
    pub fn windows(&self, cores: usize) -> (usize, usize) {
        let cores = cores.max(1);
        let to_usize = |value: NonZeroU32| usize::try_from(value.get()).unwrap_or(usize::MAX);
        let max = self.max.map_or(cores, to_usize);
        let initial = self.initial.map_or(max, to_usize);
        (initial.min(max), max)
    }

    /// Reject ranges the parser cannot express.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.target_cpu.0 <= 0.0 || self.target_cpu.0 > 100.0 {
            return Err(ConfigError::plain(
                "startup.target_cpu: must be above 0% and at most 100%",
            ));
        }
        if self.slot_timeout.0.is_zero() {
            return Err(ConfigError::plain(
                "startup.slot_timeout: must be longer than 0s",
            ));
        }
        if let (Some(initial), Some(max)) = (self.initial, self.max)
            && initial > max
        {
            return Err(ConfigError::plain(
                "startup.initial: must not be larger than startup.max",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Hostname resolution and optional system hosts-file entries.
#[schemars(extend("examples" = [{"resolver":"localhost","hosts_file":"auto"},{"resolver":"resolver","tld":"test"}]))]
pub struct HostnamesConfig {
    /// localhost or resolver in v1; public parses but is not supported yet.
    pub resolver: Resolver,
    /// Whether exact hostnames should also be written to the system hosts file.
    pub hosts_file: Option<HostsFile>,
    /// Custom top-level domain for the embedded DNS strategy.
    #[schemars(extend("examples" = ["test"]))]
    pub tld: Option<String>,
    /// Reserved public-resolver base domain; not supported yet.
    #[schemars(extend("examples" = ["dev.example.invalid"]))]
    pub base_domain: Option<String>,
    /// Reserved DNS-01 provider settings; not supported yet.
    pub dns_provider: Option<DnsProvider>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// Strategy for resolving local project hostnames; defaults to localhost.
#[schemars(extend("examples" = ["localhost","hosts_file","resolver","public"]))]
pub enum Resolver {
    /// Use names under .localhost, resolved by browsers without a custom DNS service.
    #[default]
    #[schemars(extend("examples" = ["localhost"]))]
    Localhost,
    /// Resolve exact project names through system hosts-file entries.
    #[schemars(extend("examples" = ["hosts_file"]))]
    HostsFile,
    /// Use the daemon's DNS resolver for the configured custom top-level domain.
    #[schemars(extend("examples" = ["resolver"]))]
    Resolver,
    /// Reserved public DNS and DNS-01 strategy; not supported yet.
    #[schemars(extend("examples" = ["public"]))]
    Public,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Reserved DNS-01 provider configuration; not supported yet.
#[schemars(extend("examples" = [{"cloudflare":{"token":"${{ env.CF_API_TOKEN }}"}}]))]
pub struct DnsProvider {
    /// Cloudflare DNS API credentials supplied by an environment expression.
    pub cloudflare: CloudflareDns,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Cloudflare DNS credentials for the reserved public resolver.
#[schemars(extend("examples" = [{"token":"${{ env.CF_API_TOKEN }}"}]))]
pub struct CloudflareDns {
    /// API token with permission to edit the domain's DNS records; prefer an environment expression.
    #[schemars(extend("examples" = ["${{ env.CF_API_TOKEN }}"]))]
    pub token: Templated<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Machine-wide port pool shared by all projects and lanes.
#[schemars(extend("examples" = [{"range":"20000-32767"}]))]
pub struct PortsConfig {
    /// Inclusive port range, written start-end; defaults to 20000-32767.
    #[schemars(extend("pattern" = "^[0-9]+-[0-9]+$", "examples" = ["20000-32767"]))]
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
                "ports.range: must be an ascending nonzero range, such as 20000-32767",
            )
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Certificate authority selection and requested certificate trust targets.
#[schemars(extend("examples" = [{"ca":"devlanes","trust":["system","firefox"]}]))]
pub struct TlsConfig {
    /// Use devlanes' own CA by default, or reuse mkcert's CA.
    pub ca: CaKind,
    /// Trust stores that should accept the local CA; mobile targets are ignored with a warning for now.
    #[schemars(extend("examples" = [["system","firefox"],["chrome"]]))]
    pub trust: Vec<TrustTarget>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// Certificate authority used to issue certificates for local project hostnames.
#[schemars(extend("examples" = ["devlanes","mkcert"]))]
pub enum CaKind {
    /// Use the CA managed in the devlanes home directory.
    #[default]
    #[schemars(extend("examples" = ["devlanes"]))]
    Devlanes,
    /// Reuse the CA in mkcert's CAROOT directory.
    #[schemars(extend("examples" = ["mkcert"]))]
    Mkcert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
/// Store in which the local CA should be trusted; mobile stores are not supported yet.
#[schemars(extend("examples" = ["system","firefox","chrome","ios-simulator","android-emulator"]))]
pub enum TrustTarget {
    /// Trust the local CA in the operating system's certificate store.
    #[schemars(extend("examples" = ["system"]))]
    System,
    /// Trust the local CA in Firefox's certificate database.
    #[schemars(extend("examples" = ["firefox"]))]
    Firefox,
    /// Trust the local CA in Chrome's certificate database where required.
    #[schemars(extend("examples" = ["chrome"]))]
    Chrome,
    /// Reserved iOS simulator trust target; not supported yet, so it is ignored with a warning.
    #[schemars(extend("examples" = ["ios-simulator"]))]
    IosSimulator,
    /// Reserved Android emulator trust target; not supported yet, so it is ignored with a warning.
    #[schemars(extend("examples" = ["android-emulator"]))]
    AndroidEmulator,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Ports and network access policy for the local HTTP/HTTPS proxy.
#[schemars(extend("examples" = [{"https_port":443,"http_port":80,"lan":false}]))]
pub struct ProxyConfig {
    /// HTTPS listening port; defaults to 443.
    #[schemars(extend("examples" = [443,8443]))]
    pub https_port: u16,
    /// HTTP listening port; defaults to 80 and redirects to HTTPS when TLS is enabled.
    #[schemars(extend("examples" = [80,8080]))]
    pub http_port: u16,
    /// Accept connections from other machines; defaults to loopback-only access.
    #[schemars(extend("examples" = [false,true]))]
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
/// Retention settings for stored service/task output and database log metadata.
#[schemars(extend("examples" = [{"retention":{"days":7,"max_size":"5GiB"}}]))]
pub struct LogsConfig {
    /// Age and total-size bounds for log files; the age limit also bounds database log-segment metadata.
    pub retention: LogRetention,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Disk retention bounded by both age and total log size.
#[schemars(extend("examples" = [{"days":7,"max_size":"5GiB"}]))]
pub struct LogRetention {
    /// Number of days to retain logs and database log-segment metadata; defaults to 7.
    #[schemars(extend("examples" = [7]))]
    pub days: u32,
    /// Maximum total size of retained logs; defaults to 5GiB.
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
/// Retention for monitor history used by graphs and service baselines, including database aggregates.
#[schemars(extend("examples" = [{"retention":"7d"}]))]
pub struct MetricsConfig {
    /// Maximum age of persisted monitor samples and database run summaries; defaults to 7d.
    #[schemars(extend("examples" = ["7d","30d"]))]
    pub retention: ConfigDuration,
}

impl Default for MetricsConfig {
    /// Keep a week of persisted monitor aggregates.
    fn default() -> Self {
        Self {
            retention: ConfigDuration(DEFAULT_METRICS_RETENTION),
        }
    }
}

/// How devlanes uses the terminal, or (from older configs) a terminal app
/// name, which is ignored.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = [{"progress":"auto"}]))]
pub enum TerminalConfig {
    /// A terminal app name from when devlanes opened units in a new terminal
    /// window; that feature is gone, so the name is ignored with a warning.
    #[schemars(extend("examples" = ["ghostty"]))]
    Legacy(String),
    /// How devlanes uses the terminal's own progress bar.
    #[schemars(extend("examples" = [{"progress":"never"}]))]
    Settings(TerminalSettings),
}

/// How devlanes uses the terminal: its progress bar, tab title, links, notifications and
/// marks.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"progress":"auto"},{"progress":"never","title":"never"}]))]
pub struct TerminalSettings {
    /// A terminal app from older configs; ignored with a warning.
    #[serde(skip_serializing)]
    #[schemars(skip)]
    pub app: Option<String>,
    /// Show start progress in the terminal tab's progress bar (OSC 9;4); defaults to auto.
    #[schemars(extend("examples" = ["auto","never"]))]
    pub progress: TerminalProgress,
    /// Whether the terminal tab's title says what devlanes is doing (`devlanes · acme@0 ·
    /// starting 3/8` in the TUI, `devlanes start acme@0 · 3/8` or `devlanes logs -f api`
    /// for long commands); defaults to `auto`. The previous title is saved on the
    /// terminal's title stack and put back on exit, so `auto` sets it only where that
    /// stack exists: xterm, iTerm2, kitty, Ghostty and VTE terminals such as GNOME
    /// Terminal. Never inside tmux or screen, which manage titles themselves.
    /// `DEVLANES_TERM_TITLE=0` or `1` overrides this setting.
    #[schemars(extend("examples" = ["auto","never"]))]
    pub title: TerminalFeature,
    /// Whether service URLs, config error locations and log or bundle paths are
    /// clickable links (the OSC 8 sequence); defaults to `auto`, which writes them in
    /// iTerm2, kitty, WezTerm, Ghostty, Windows Terminal, VTE terminals, Konsole,
    /// Alacritty and foot. Elsewhere the same text prints without a link.
    /// `DEVLANES_TERM_LINKS=0` or `1` overrides this setting.
    #[schemars(extend("examples" = ["auto","never"]))]
    pub links: TerminalFeature,
    /// Whether a start that took a while finishing or failing, and a running service
    /// crashing, show a desktop notification while the terminal is not focused; defaults
    /// to `auto`. The TUI knows focus from the terminal; a command such as `devlanes
    /// start` does not, so with `auto` it notifies only in kitty, which itself skips a
    /// focused window. Terminals without notifications get the bell. At most one
    /// notification per 10 seconds. `DEVLANES_TERM_NOTIFICATIONS=0` or `1` overrides
    /// this setting.
    #[schemars(extend("examples" = ["auto","never"]))]
    pub notifications: TerminalFeature,
    /// Whether long command output (logs of several services, failed starts) marks where
    /// each section starts, so the terminal's jump-to-mark or jump-to-prompt keys move
    /// between them; defaults to `auto`: marks in iTerm2, and prompt marks (OSC 133) in
    /// WezTerm, Ghostty, kitty, foot and Windows Terminal. `DEVLANES_TERM_MARKS=0` or
    /// `1` overrides this setting.
    #[schemars(extend("examples" = ["auto","never"]))]
    pub marks: TerminalFeature,
}

/// When devlanes uses a terminal feature (title, links, notifications, marks).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(extend("examples" = ["auto","always","never"]))]
pub enum TerminalFeature {
    /// Only in terminals known to support it, on human output to a terminal; never
    /// inside tmux, screen, CI or a dumb terminal.
    #[default]
    #[schemars(extend("examples" = ["auto"]))]
    Auto,
    /// In every terminal, tmux included; a terminal without support may ignore it, or
    /// keep a title devlanes cannot put back.
    #[schemars(extend("examples" = ["always"]))]
    Always,
    /// Never use it.
    #[schemars(extend("examples" = ["never"]))]
    Never,
}

/// When start progress goes to the terminal tab's progress bar. Only a terminal
/// session in human output ever gets it; `DEVLANES_TERM_PROGRESS=0` or `1` overrides this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(extend("examples" = ["auto","always","never"]))]
pub enum TerminalProgress {
    /// Only in terminals known to draw it (Ghostty, WezTerm, recent iTerm2, Windows Terminal, ConEmu, Ptyxis, Konsole), never inside tmux or screen.
    #[default]
    #[schemars(extend("examples" = ["auto"]))]
    Auto,
    /// In any terminal, also kitty 0.47 or later and inside tmux (needs `allow-passthrough on`).
    #[schemars(extend("examples" = ["always"]))]
    Always,
    /// Never.
    #[schemars(extend("examples" = ["never"]))]
    Never,
}

impl TerminalConfig {
    /// The progress-bar setting; the legacy app name keeps it on auto.
    pub fn progress(&self) -> TerminalProgress {
        self.settings().progress
    }

    /// Every terminal setting; the legacy app name keeps them all on auto.
    pub fn settings(&self) -> TerminalSettings {
        match self {
            Self::Legacy(_) => TerminalSettings::default(),
            Self::Settings(settings) => TerminalSettings {
                app: None,
                ..settings.clone()
            },
        }
    }

    /// A terminal app name an older config set, now ignored.
    fn legacy_app(&self) -> Option<&str> {
        match self {
            Self::Legacy(app) => Some(app),
            Self::Settings(settings) => settings.app.as_deref(),
        }
    }
}

impl UserConfig {
    /// Reject the deferred public resolver and invalid machine-wide resource ranges.
    /// Messages start with the offending field path.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.state.max_size.0 == 0 {
            return Err(ConfigError::plain(
                "state.max_size: must be greater than zero",
            ));
        }
        for (path, deferred) in [
            (
                "hostnames.resolver",
                self.hostnames.resolver == Resolver::Public,
            ),
            (
                "hostnames.base_domain",
                self.hostnames.base_domain.is_some(),
            ),
            (
                "hostnames.dns_provider",
                self.hostnames.dns_provider.is_some(),
            ),
        ] {
            if deferred {
                return Err(ConfigError::plain(format!(
                    "{path}: the public resolver and DNS-01 settings are not supported yet"
                )));
            }
        }
        self.ports.bounds()?;
        for (path, port) in [
            ("proxy.http_port", self.proxy.http_port),
            ("proxy.https_port", self.proxy.https_port),
        ] {
            if port == 0 {
                return Err(ConfigError::plain(format!("{path}: must be nonzero")));
            }
        }
        if self.proxy.http_port == self.proxy.https_port {
            return Err(ConfigError::plain(
                "proxy.https_port: must differ from proxy.http_port",
            ));
        }
        if self.hostnames.resolver == Resolver::Resolver
            && self.hostnames.tld.as_ref().is_none_or(String::is_empty)
        {
            return Err(ConfigError::plain(
                "hostnames.tld: required for the resolver strategy",
            ));
        }
        self.startup.validate()?;
        Ok(())
    }

    /// Settings that [`parse_user`] and [`load_user`] ignored, such as mobile trust targets,
    /// for display in `devlanes doctor`.
    pub fn warnings(&self) -> &[crate::ConfigWarning] {
        &self.warnings
    }

    /// Warns about a terminal app name from older configs: devlanes no longer
    /// opens units in a new terminal window.
    fn ignore_legacy_terminal(&mut self) {
        let Some(app) = self.terminal.as_ref().and_then(TerminalConfig::legacy_app) else {
            return;
        };
        let path = match self.terminal {
            Some(TerminalConfig::Legacy(_)) => "terminal",
            _ => "terminal.app",
        };
        self.warnings.push(crate::ConfigWarning {
            path: path.into(),
            message: format!(
                "devlanes no longer opens units in a terminal app; {app} is ignored and can be removed"
            ),
        });
    }

    /// Drop deferred trust targets with a warning instead of passing them to trust adapters.
    fn ignore_deferred_trust(&mut self) {
        for (index, target) in self.tls.trust.iter().enumerate() {
            if matches!(
                target,
                TrustTarget::IosSimulator | TrustTarget::AndroidEmulator
            ) {
                self.warnings.push(crate::ConfigWarning {
                    path: format!("tls.trust.{index}"),
                    message: "mobile certificate trust is not supported yet; target ignored".into(),
                });
            }
        }
        self.tls.trust.retain(|target| {
            !matches!(
                target,
                TrustTarget::IosSimulator | TrustTarget::AndroidEmulator
            )
        });
    }
}

/// Parse machine settings separately from project settings and validate v1 support.
pub fn parse_user(source: &str) -> Result<UserConfig, ConfigError> {
    parse_user_named("config.yaml", source)
}

/// Load an explicitly selected user config file; absence is an I/O error.
pub fn load_user(path: impl AsRef<Path>) -> Result<UserConfig, ConfigError> {
    let path = path.as_ref();
    let source = std::fs::read_to_string(path).map_err(|e| ConfigError::io(path, e))?;
    parse_user_named(&path.display().to_string(), &source)
}

/// Like [`load_user`], but a problem drops only the key it is about: an unknown
/// key or an invalid value is left out (its default applies) and comes back as a
/// located error, and every valid key is kept. Only a file that cannot be read
/// or is not YAML at all yields the defaults with that one error.
pub fn load_user_lenient(path: impl AsRef<Path>) -> (UserConfig, Vec<ConfigError>) {
    let path = path.as_ref();
    match std::fs::read_to_string(path) {
        Ok(source) => parse_user_lenient(&path.display().to_string(), &source),
        Err(error) => (UserConfig::default(), vec![ConfigError::io(path, error)]),
    }
}

/// Keys dropped before giving up and using the defaults for everything.
const LENIENT_ATTEMPTS: usize = 64;

/// [`load_user_lenient`] over text; `file` names it in locations.
pub fn parse_user_lenient(file: &str, source: &str) -> (UserConfig, Vec<ConfigError>) {
    let mut value: serde_json::Value = match crate::loader::parse_yaml(file, source) {
        Ok(value) => value,
        Err(error) => return (UserConfig::default(), vec![error]),
    };
    // An empty file is YAML null; it means "all defaults".
    if value.is_null() {
        value = serde_json::Value::Object(serde_json::Map::new());
    }
    let mut issues = Vec::new();
    for _ in 0..LENIENT_ATTEMPTS {
        let (path, message) = match serde_path_to_error::deserialize::<_, UserConfig>(&value) {
            Ok(mut config) => match config.validate() {
                Ok(()) => {
                    config.ignore_deferred_trust();
                    config.ignore_legacy_terminal();
                    return (config, issues);
                }
                Err(error) => match error.message.split_once(": ") {
                    Some((path, message)) => (path.to_owned(), message.to_owned()),
                    None => (String::new(), error.message),
                },
            },
            Err(error) => {
                let message = error.inner().to_string();
                let mut path = error.path().to_string();
                // An unknown key is reported at its parent; name the key itself.
                if let Some((_, rest)) = message.split_once("unknown field `")
                    && let Some((key, _)) = rest.split_once('`')
                    && !path.ends_with(key)
                {
                    path = if path == "." || path.is_empty() {
                        key.to_owned()
                    } else {
                        format!("{path}.{key}")
                    };
                }
                (path, message)
            }
        };
        if path.is_empty() || path == "." || !drop_key(&mut value, &path) {
            break;
        }
        issues.push(crate::loader::locate(
            ConfigError::plain(format!("{path}: {message}; the default applies")),
            file,
            source,
            None,
        ));
    }
    issues.push(ConfigError::plain(
        "config.yaml: too many problems to keep any setting; every default applies",
    ));
    (UserConfig::default(), issues)
}

/// Removes the key at the dotted `path` from `value`. When the key is absent
/// (a validation error about a value that must exist), the nearest present
/// ancestor goes instead, so its section falls back to defaults. False when
/// nothing could be removed.
fn drop_key(value: &mut serde_json::Value, path: &str) -> bool {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    for depth in (1..=segments.len()).rev() {
        let (parents, last) = segments[..depth].split_at(depth - 1);
        let pointer: String = parents
            .iter()
            .map(|segment| format!("/{}", segment.replace('~', "~0").replace('/', "~1")))
            .collect();
        let Some(node) = value.pointer_mut(&pointer) else {
            continue;
        };
        let removed = match node {
            serde_json::Value::Object(map) => map.remove(last[0]).is_some(),
            serde_json::Value::Array(items) => match last[0].parse::<usize>() {
                Ok(index) if index < items.len() => {
                    items.remove(index);
                    true
                }
                _ => false,
            },
            _ => false,
        };
        if removed {
            return true;
        }
    }
    false
}

/// Parse, validate with errors located at their field, then drop deferred settings.
fn parse_user_named(file: &str, source: &str) -> Result<UserConfig, ConfigError> {
    let mut config: UserConfig = crate::loader::parse_yaml(file, source)?;
    config
        .validate()
        .map_err(|e| crate::loader::locate(e, file, source, None))?;
    config.ignore_deferred_trust();
    config.ignore_legacy_terminal();
    Ok(config)
}
