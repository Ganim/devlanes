use crate::{ByteSize, ConfigDuration, ConfigError, HostsFile, Percent, Templated};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{num::NonZeroU32, path::Path, time::Duration};

/// Finished unit/task history lifetime, duplicated from the state crate to avoid a dependency.
/// The binary's `config_and_state_retention_defaults_match` test keeps these independent
/// definitions aligned.
pub const DEFAULT_RUN_HISTORY_RETENTION: Duration = Duration::from_secs(7 * 86_400);
/// Monitor history lifetime, duplicated from the state crate and checked by the binary test.
pub const DEFAULT_METRICS_RETENTION: Duration = Duration::from_secs(7 * 86_400);
/// Disconnected remote-host lifetime, duplicated from the state crate and checked by the binary test.
pub const DEFAULT_REMOTE_HOST_RETENTION: Duration = Duration::from_secs(90 * 86_400);
/// Database and SQLite sidecar budget, duplicated from the state crate and checked by the binary test.
pub const DEFAULT_MAX_SIZE_BYTES: u64 = 512 * 1024 * 1024;

/// Settings for this machine, shared by every project, read from `config.yaml` in the
/// devlanes home (`~/.devlanes`, or `$DEVLANES_HOME`). The daemon reads the file when it
/// starts, so most changes need `devlanes daemon restart`; `start`, `tui` and `terminal`
/// apply to the next command. An unknown key or invalid value stops the daemon from starting, with
/// the file and line to fix; `devlanes doctor` checks the file without restarting.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"hostnames":{"resolver":"localhost"},"ports":{"range":"20000-32767"},"terminal":{"progress":"auto"}}]))]
pub struct UserConfig {
    /// Settings that loading ignored, reported by [`UserConfig::warnings`].
    #[serde(skip)]
    #[schemars(skip)]
    warnings: Vec<crate::ConfigWarning>,
    /// How lane hostnames such as `api.acme-2.localhost` resolve to this machine: the
    /// strategy (`resolver`), its `tld`, and whether names also go into `/etc/hosts`
    /// (`hosts_file`). The defaults need no setup for browsers that resolve `*.localhost`.
    pub hostnames: HostnamesConfig,
    /// Port range that every lane of every project takes its block of ports from, so
    /// lanes never collide. Defaults to `20000-32767`.
    pub ports: PortsConfig,
    /// Where lane data folders (`lane.data_path`) live on this machine, and how long a
    /// lane may sit without activity before devlanes throttles or freezes it.
    pub lanes: UserLanesConfig,
    /// Which certificate authority signs the proxy's HTTPS certificates (`ca`), and which
    /// stores `devlanes certs trust` installs it into (`trust`).
    pub tls: TlsConfig,
    /// Ports of the daemon's HTTP/HTTPS proxy, which serves every `hosts` entry, and
    /// whether other devices on the network may connect to it.
    pub proxy: ProxyConfig,
    /// How much service output devlanes keeps under `~/.devlanes/logs`, by age and total size.
    pub logs: LogsConfig,
    /// How long CPU and memory history stays in the daemon's database.
    pub metrics: MetricsConfig,
    /// How long the database keeps records of finished service and task runs, and of
    /// remote boxes that no longer connect.
    pub history: HistoryConfig,
    /// Size budget for the daemon's database; past it, the oldest metrics and history are
    /// deleted before their retention ends.
    pub state: StateConfig,
    /// How many services may be starting at the same time across all lanes and
    /// projects, so a large start does not overload the machine. Defaults to one start
    /// per CPU core.
    pub startup: StartupConfig,
    /// What a plain `devlanes start` (no services, no `--profile`) brings up: the
    /// services the lane ran before, or its default selection.
    pub start: StartConfig,
    /// Terminal integration of the CLI, written as a mapping with `progress` (the tab's
    /// progress bar during starts). A plain string, the terminal app name older versions
    /// used, is still accepted but ignored with a warning.
    pub terminal: Option<TerminalConfig>,
    /// Daily check for a newer devlanes release; nothing is downloaded or installed.
    pub updates: UpdatesConfig,
    /// Whether start commands open the terminal UI, how it looks, and what quitting it
    /// does with running lanes.
    pub tui: TuiConfig,
    /// The daemon's lifetime: whether it stops on its own once the last TUI closes and
    /// nothing runs.
    pub daemon: DaemonConfig,
}

/// What a plain `devlanes start`, or `s` on a lane in the TUI, starts. Changes apply
/// to the next command.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"restore":true},{"restore":false}]))]
pub struct StartConfig {
    /// Bring back the services the lane ran before, with the same profiles, so a
    /// reboot or daemon crash costs one `devlanes start`; defaults to true. A lane
    /// stopped as a whole with `devlanes stop` has nothing to bring back and starts
    /// its `default` profile. Set false to always start the `default` profile (or
    /// every `autostart` service). `lane resume` and `daemon restart` restore either way.
    #[schemars(extend("examples" = [true,false]))]
    pub restore: bool,
}

impl Default for StartConfig {
    /// Restores what ran before.
    fn default() -> Self {
        Self { restore: true }
    }
}

/// Terminal UI settings: when it opens, its colors and symbols, and what quitting does
/// with running lanes. Changes apply to the next command.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"open_on_start":false},{"theme":"light","glyphs":"ascii"},{"on_quit":"keep"}]))]
pub struct TuiConfig {
    /// Open the TUI for `start`, `restart`, `lane resume` and `lane new --start` when they
    /// run at a terminal; defaults to true. Set false to have them print text instead, as
    /// `--no-ui` or `DEVLANES_NO_UI=1` do.
    #[schemars(extend("examples" = [true,false]))]
    pub open_on_start: bool,
    /// Color palette of the TUI; defaults to `auto`, which picks one from what the terminal
    /// reports. Choose `dark` or `light` when auto guesses wrong; `NO_COLOR` in the
    /// environment turns colors off whatever this says.
    pub theme: TuiTheme,
    /// Characters the TUI draws states, markers and the spinner with; defaults to `auto`.
    /// Set `ascii` when your font shows boxes or misaligned symbols.
    pub glyphs: TuiGlyphs,
    /// What `q` or Ctrl+C does while lanes of the project on screen are running; defaults
    /// to `ask`. Ticking "remember" in the quit dialog writes the choice here.
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

/// What quitting the TUI does with the running lanes of the project on screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(extend("examples" = ["ask","keep","stop"]))]
pub enum TuiOnQuit {
    /// Show a dialog each time to choose between keeping the lanes running and stopping them.
    #[default]
    #[schemars(extend("examples" = ["ask"]))]
    Ask,
    /// Close the TUI at once; every lane keeps running in the daemon.
    #[schemars(extend("examples" = ["keep"]))]
    Keep,
    /// Stop the running lanes of the project on screen, then close the TUI. Lanes of other
    /// projects keep running.
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

/// Color palette of the TUI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(extend("examples" = ["auto","dark","light","ansi"]))]
pub enum TuiTheme {
    /// Use the true-color dark or light palette when the terminal reports true color
    /// (`COLORTERM`) and its background (`COLORFGBG`); otherwise the terminal's 16 colors.
    #[default]
    #[schemars(extend("examples" = ["auto"]))]
    Auto,
    /// Always use the true-color palette made for a dark background.
    #[schemars(extend("examples" = ["dark"]))]
    Dark,
    /// Always use the true-color palette made for a light background.
    #[schemars(extend("examples" = ["light"]))]
    Light,
    /// Use only the terminal's 16 named colors, so the TUI follows the terminal's color scheme.
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

/// Character set the TUI draws its symbols with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(extend("examples" = ["auto","unicode","ascii"]))]
pub enum TuiGlyphs {
    /// Unicode, or ASCII when the locale (the first of `LC_ALL`, `LC_CTYPE` and `LANG`
    /// that is set) is not UTF-8.
    #[default]
    #[schemars(extend("examples" = ["auto"]))]
    Auto,
    /// Always Unicode symbols, such as ● ✕ ▾ and a braille spinner.
    #[schemars(extend("examples" = ["unicode"]))]
    Unicode,
    /// Plain ASCII only, for fonts or terminals that draw Unicode symbols badly.
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

/// Machine settings for lanes: where their data folders live, and when idle lanes are
/// throttled or frozen. Each lane gets its own data folder at
/// `<data_dir>/<project>/<lane id>`, available as `${{ lane.data_path }}` and
/// `DEVLANES_LANE_DATA`. devlanes creates it when the lane is created and before each
/// start, and `devlanes lane clean` deletes it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"data_dir":"~/.devlanes/data"},{"throttle_after":"5m","freeze_after":"15m"},{"throttle_after":"0s","freeze_after":"1h"}]))]
pub struct UserLanesConfig {
    /// Folder that holds every lane's data folder, one subfolder per project; defaults to
    /// `data` in the devlanes home (`~/.devlanes/data`). Must be an absolute path or start
    /// with `~/`. The daemon reads it when it starts. After a change and a daemon restart,
    /// lanes use new, empty folders under the new root; move the old folders there by hand
    /// to keep their data.
    #[schemars(extend("examples" = ["~/.devlanes/data","/Volumes/fast/devlanes"]))]
    pub data_dir: Option<String>,
    /// After this long without activity, a lane's processes run at background priority:
    /// lower CPU priority, and efficiency cores on machines that have them. They keep
    /// serving requests. Activity is a request through the proxy, a new connection to one
    /// of the lane's ports, the lane's services using CPU, or a devlanes command on the
    /// lane; any of it restores normal priority and restarts the timer. macOS and Windows
    /// only; Linux has no throttling, so lanes there stay at normal priority until
    /// `freeze_after`. When both are set above `0s`, it must be shorter than
    /// `freeze_after`. A running service with `services.<name>.keep_awake: true` keeps its
    /// whole lane at normal priority. Defaults to `5m`; `0s` turns throttling off. The
    /// daemon reads it when it starts.
    #[schemars(extend("examples" = ["5m","0s"]))]
    pub throttle_after: ConfigDuration,
    /// After this long without activity, devlanes freezes a lane: it suspends every
    /// process of the lane and pauses the lane's own containers. Shared infra is never
    /// paused. Ports and process state are kept, so a frozen lane still counts as running.
    /// Activity is a request through the proxy, a new connection to one of the lane's
    /// ports, the lane's services using CPU, or a devlanes command on the lane; any of it
    /// resumes a frozen lane and restarts the timer. A request through the proxy resumes
    /// the lane at once and then goes through; a direct connection to a port is noticed
    /// within about half a second. When both are set above `0s`, it must be longer than
    /// `throttle_after` (`5m` unless set). A running service with
    /// `services.<name>.keep_awake: true` keeps its whole lane awake. Defaults to `15m`;
    /// `0s` turns freezing off. The daemon reads it when it starts.
    #[schemars(extend("examples" = ["15m","1h","0s"]))]
    pub freeze_after: ConfigDuration,
}

/// Folder under the devlanes home that holds lane data folders by default.
pub const DEFAULT_DATA_DIR: &str = "data";

/// Idle time before a lane is throttled when `lanes.throttle_after` is absent.
pub const DEFAULT_THROTTLE_AFTER: Duration = Duration::from_secs(5 * 60);

/// Idle time before a lane is frozen when `lanes.freeze_after` is absent.
pub const DEFAULT_FREEZE_AFTER: Duration = Duration::from_secs(15 * 60);

impl Default for UserLanesConfig {
    /// Data folders under the devlanes home; idle lanes throttle at 5m and freeze at 15m.
    fn default() -> Self {
        Self {
            data_dir: None,
            throttle_after: ConfigDuration(DEFAULT_THROTTLE_AFTER),
            freeze_after: ConfigDuration(DEFAULT_FREEZE_AFTER),
        }
    }
}

impl UserLanesConfig {
    /// Idle time before a lane is throttled, or `None` when `throttle_after: 0s`
    /// turns throttling off.
    pub fn throttle_after(&self) -> Option<Duration> {
        Some(self.throttle_after.0).filter(|after| !after.is_zero())
    }

    /// Idle time before a lane is frozen, or `None` when `freeze_after: 0s` turns
    /// freezing off.
    pub fn freeze_after(&self) -> Option<Duration> {
        Some(self.freeze_after.0).filter(|after| !after.is_zero())
    }

    /// The folder holding lane data folders: `data_dir` with `~/` expanded from `HOME`
    /// (on Windows also `~\`, from the profile directory), or `<home>/data`, where
    /// `home` is the devlanes home.
    pub fn data_root(&self, home: &Path) -> std::path::PathBuf {
        let Some(dir) = self.data_dir.as_deref() else {
            return home.join(DEFAULT_DATA_DIR);
        };
        match (below_home(dir), devlanes_core::paths::user_home()) {
            (Some(rest), Some(user_home)) => user_home.join(rest),
            _ => std::path::PathBuf::from(dir),
        }
    }

    /// Rejects relative folders, which would depend on the daemon's working directory,
    /// and a throttle that would start after the lane is already frozen.
    fn validate(&self) -> Result<(), ConfigError> {
        match self.data_dir.as_deref() {
            None => {}
            Some(dir) if below_home(dir).is_some() || Path::new(dir).is_absolute() => {}
            Some(_) => {
                return Err(ConfigError::plain(
                    "lanes.data_dir: must be an absolute path or start with `~/`",
                ));
            }
        }
        if let (Some(throttle), Some(freeze)) = (self.throttle_after(), self.freeze_after())
            && throttle >= freeze
        {
            // The message's leading key is where the error is located and what a lenient
            // load drops, so name the key the user most likely changed: a lone
            // `freeze_after` below the default throttle time should not point at a
            // `throttle_after` the file never set.
            let (throttle_text, freeze_text) = (
                crate::setting::short_duration_of(self.throttle_after),
                crate::setting::short_duration_of(self.freeze_after),
            );
            let message = if throttle == DEFAULT_THROTTLE_AFTER {
                format!(
                    "lanes.freeze_after: {freeze_text} must be longer than lanes.throttle_after, \
                     which is {throttle_text}; lower lanes.throttle_after or set it to 0s"
                )
            } else {
                format!(
                    "lanes.throttle_after: {throttle_text} must be shorter than \
                     lanes.freeze_after, which is {freeze_text}"
                )
            };
            return Err(ConfigError::plain(message));
        }
        Ok(())
    }
}

/// The part of a `~/...` folder below the user's home.
#[cfg(unix)]
fn below_home(dir: &str) -> Option<&str> {
    dir.strip_prefix("~/")
}

/// The part of a `~/...` or `~\...` folder below the user's home.
#[cfg(windows)]
fn below_home(dir: &str) -> Option<&str> {
    dir.strip_prefix("~/").or_else(|| dir.strip_prefix("~\\"))
}

/// Release checks: the daemon reads the latest release's version once a day and
/// commands print a one-line notice. Nothing is downloaded or installed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"check":false}]))]
pub struct UpdatesConfig {
    /// Have the daemon read the latest release's version from GitHub once a day; when it
    /// is newer, commands print the upgrade command on one line, at most once a day.
    /// Defaults to true; `DEVLANES_NO_UPDATE_CHECK=1` or a CI environment also turns it off.
    #[schemars(extend("examples" = [true,false]))]
    pub check: bool,
}

impl Default for UpdatesConfig {
    /// Checks are on unless turned off.
    fn default() -> Self {
        Self { check: true }
    }
}

/// The daemon's own lifetime.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"exit_when_idle":false}]))]
pub struct DaemonConfig {
    /// Stop the daemon about 30 seconds after the last TUI closes when no service, task
    /// or start is running; the next devlanes command starts it again. Containers keep
    /// running either way. Defaults to true. Only a daemon a command started on demand
    /// exits; one installed with `devlanes daemon install` keeps running.
    #[schemars(extend("examples" = [true,false]))]
    pub exit_when_idle: bool,
}

impl Default for DaemonConfig {
    /// Exits when idle unless turned off.
    fn default() -> Self {
        Self {
            exit_when_idle: true,
        }
    }
}

/// How long the daemon's database keeps records of finished runs and of remote boxes.
/// An hourly cleanup deletes what is older.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"retention":"7d","remote_hosts_retention":"90d"}]))]
pub struct HistoryConfig {
    /// How long records of finished service and task runs (start and end times, exit
    /// status) are kept after the run ends; defaults to `7d` and must be longer than
    /// `0s`. A run that still has log index entries or a metrics summary in the database
    /// stays until those expire, and `state.max_size` can delete runs sooner.
    #[schemars(extend("examples" = ["7d","30d"]))]
    pub retention: ConfigDuration,
    /// How long a remote box stays listed by `devlanes remote ls` after its last
    /// successful connection; defaults to `90d` and must be longer than `0s`. A record
    /// without a connection time is never deleted by age, and `state.max_size` can
    /// delete records sooner.
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

/// Size budget for the daemon's SQLite database, enforced by its hourly cleanup.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("examples" = [{"max_size":"512MiB"}]))]
pub struct StateConfig {
    /// Size the database (`state.db` with its WAL and shared-memory files) should stay
    /// under; defaults to `512MiB` and must be above zero. When it is larger, the cleanup
    /// deletes the oldest metric samples first, then the oldest history (finished runs,
    /// log index entries, remote boxes), even before their retention ends. Lanes, ports
    /// and running units are never deleted, so the file can stay above the budget.
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

/// How the number of services allowed to start at once is chosen.
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
    /// Allow `max` starts at a time, never more or fewer. A start also waits while free
    /// memory cannot fit what it needs.
    #[schemars(extend("examples" = ["fixed"]))]
    Fixed,
    /// Start every service immediately, with no limit and no memory check.
    #[schemars(extend("examples" = ["all"]))]
    All,
}

impl Default for StartupMode {
    /// [`DEFAULT_STARTUP_MODE`].
    fn default() -> Self {
        DEFAULT_STARTUP_MODE
    }
}

/// Pacing of service starts, shared by every lane and project on this machine. Starts
/// queue first come, first served; only spawning waits, running processes are never limited.
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
    /// Most services starting at once: the constant window in `fixed` mode and the
    /// ceiling in `adaptive` mode. Defaults to the number of CPU cores; `all` ignores it.
    #[schemars(extend("examples" = [8]))]
    pub max: Option<NonZeroU32>,
    /// System CPU use below which `adaptive` mode adds slots; defaults to `75%`, and must
    /// be above 0% and at most 100%. Lower it to keep the machine responsive during large
    /// starts; other modes ignore it.
    #[schemars(extend("examples" = ["75%", "80%" ]))]
    pub target_cpu: Percent,
    /// Only for services without a ready check (`ready: started`, the default for a
    /// service without `hosts`): such a service gives its slot back once its process tree
    /// uses less than this share of one core for 2 s, meaning it has finished starting up.
    /// Services with a `tcp`, `http`, `log` or `cmd` check keep their slot until the check
    /// passes instead. Defaults to `10%`.
    #[schemars(extend("examples" = ["10%","12.5%" ]))]
    pub settle_cpu: Percent,
    /// Longest a starting service holds its slot; after it, the next queued service may
    /// start even if this one is not ready, so services waiting on each other cannot
    /// block the queue. Defaults to `20s` and must be above zero; the service itself keeps
    /// starting.
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
/// How lane hostnames resolve to this machine, and whether they are also written to
/// `/etc/hosts` for programs that cannot resolve them otherwise.
#[schemars(extend("examples" = [{"resolver":"localhost","hosts_file":"auto"},{"resolver":"resolver","tld":"test"}]))]
pub struct HostnamesConfig {
    /// Which domain lanes get and what makes their hostnames resolve; defaults to
    /// `localhost`. `resolver` also needs `tld` and a one-time
    /// `devlanes certs trust --resolver`; `public` is not supported yet.
    pub resolver: Resolver,
    /// Whether lane hostnames are also written to `/etc/hosts`, for programs that do not
    /// resolve `*.localhost` themselves (workerd, musl or pure-Go binaries); defaults to
    /// `auto`, which writes them only when a probe at daemon start shows the system
    /// resolver fails `*.localhost`. `never` here wins over every project; otherwise a
    /// project's `lanes.hosts_file` replaces this value for that project. Writing needs the
    /// helper `devlanes certs trust --hosts` installs, only exact names are written (no
    /// wildcards), and `resolver: hosts_file` ignores this key.
    pub hosts_file: Option<HostsFile>,
    /// Top-level domain for the `resolver` strategy, such as `test`; required there. It
    /// must be `test`, `internal`, `home.arpa` or a domain under one of them (or under
    /// `localhost`), made of valid DNS labels, because any other name could belong to a
    /// real site; the file is rejected otherwise, whatever the strategy. A new devlanes
    /// CA may sign names under the `tld` set when it is created, whatever the strategy,
    /// so switching to another `tld` later needs a new CA.
    #[schemars(extend("examples" = ["test"]))]
    pub tld: Option<String>,
    /// Reserved for the planned `public` resolver; any value is rejected for now.
    #[schemars(extend("examples" = ["dev.example.invalid"]))]
    pub base_domain: Option<String>,
    /// Reserved for the planned `public` resolver's DNS-01 certificates; any value is
    /// rejected for now.
    pub dns_provider: Option<DnsProvider>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// Strategy that makes lane hostnames resolve to this machine.
#[schemars(extend("examples" = ["localhost","hosts_file","resolver","public"]))]
pub enum Resolver {
    /// Lane domains end in `.localhost` (`acme.localhost`, `acme-2.localhost`), which
    /// browsers and many system resolvers send to loopback by themselves. `hosts_file`
    /// covers programs that do not.
    #[default]
    #[schemars(extend("examples" = ["localhost"]))]
    Localhost,
    /// Keep `.localhost` names and always write exact hostnames to `/etc/hosts`, as
    /// `hosts_file: always` does, whatever `hosts_file` says; a project's
    /// `lanes.hosts_file` still takes precedence. Needs the helper from
    /// `devlanes certs trust --hosts`.
    #[schemars(extend("examples" = ["hosts_file"]))]
    HostsFile,
    /// Lane domains become `<project>.<tld>` and `<project>-<N>.<tld>`, answered by a DNS
    /// server the daemon runs on `127.0.0.1:15353`, so wildcard hostnames resolve too for
    /// programs that use the system resolver. Needs `tld` and a one-time
    /// `devlanes certs trust --resolver`; projects that set `lanes.vars.domain` or
    /// `lanes.matrix` keep their own domains.
    #[schemars(extend("examples" = ["resolver"]))]
    Resolver,
    /// Reserved for public DNS names with DNS-01 certificates; not supported yet, so the
    /// config is rejected.
    #[schemars(extend("examples" = ["public"]))]
    Public,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Reserved DNS-01 provider settings for the planned `public` resolver; not supported yet.
#[schemars(extend("examples" = [{"cloudflare":{"token":"${{ env.CF_API_TOKEN }}"}}]))]
pub struct DnsProvider {
    /// Reserved: Cloudflare credentials for DNS-01 certificates; not supported yet.
    pub cloudflare: CloudflareDns,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Reserved Cloudflare DNS credentials for the planned `public` resolver; not supported yet.
#[schemars(extend("examples" = [{"token":"${{ env.CF_API_TOKEN }}"}]))]
pub struct CloudflareDns {
    /// Reserved: Cloudflare API token allowed to edit the domain's DNS records. Write an
    /// environment expression such as `${{ env.CF_API_TOKEN }}`, not the token itself.
    #[schemars(extend("examples" = ["${{ env.CF_API_TOKEN }}"]))]
    pub token: Templated<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Machine-wide pool every lane's block of ports comes from.
#[schemars(extend("examples" = [{"range":"20000-32767"}]))]
pub struct PortsConfig {
    /// Ports devlanes may hand out, written `start-end` (both included, start above 0);
    /// defaults to `20000-32767`, below Linux's usual ephemeral range so the OS does not
    /// pick them for outgoing connections. Each lane of each project gets its own block of
    /// consecutive ports from it, so a smaller range fits fewer lanes.
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
/// Certificate authority behind the proxy's HTTPS certificates, and the stores
/// `devlanes certs trust` installs it into.
#[schemars(extend("examples" = [{"ca":"devlanes","trust":["system","firefox"]}]))]
pub struct TlsConfig {
    /// Which CA signs the certificate for each lane hostname; defaults to `devlanes`.
    /// Choose `mkcert` when your browsers already trust mkcert's CA.
    pub ca: CaKind,
    /// Stores `devlanes certs trust` installs the CA into and `certs untrust` removes it
    /// from; an empty list means `[system]`. On Linux, browsers do not read the system
    /// store, so add `chrome` and `firefox` there; inside WSL add `windows` for browsers on
    /// Windows. `devlanes doctor` shows each store's state.
    #[schemars(extend("examples" = [["system","firefox"],["chrome"],["system","windows"]]))]
    pub trust: Vec<TrustTarget>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// Certificate authority that signs HTTPS certificates for lane hostnames.
#[schemars(extend("examples" = ["devlanes","mkcert"]))]
pub enum CaKind {
    /// devlanes creates its own CA in `~/.devlanes/ca` on first use, limited to names under
    /// `localhost`, `test`, `internal`, `home.arpa` and `hostnames.tld`, so it cannot sign
    /// for real sites. Trust it once with `devlanes certs trust`.
    #[default]
    #[schemars(extend("examples" = ["devlanes"]))]
    Devlanes,
    /// Load mkcert's existing root CA (from `mkcert -CAROOT`) instead, so browsers that
    /// already trust it need nothing new. HTTPS stays off when `mkcert` is not on `PATH`;
    /// the mkcert CA has no name limits, and `certs untrust` leaves it trusted.
    #[schemars(extend("examples" = ["mkcert"]))]
    Mkcert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
/// Certificate store that `devlanes certs trust` installs the CA into.
#[schemars(extend("examples" = ["system","firefox","chrome","windows","ios-simulator","android-emulator"]))]
pub enum TrustTarget {
    /// The OS store: the login keychain on macOS, read by Safari, Chrome, Edge and
    /// `/usr/bin/curl`; on Linux, the distribution's CA anchors, updated through sudo.
    #[schemars(extend("examples" = ["system"]))]
    System,
    /// Every Firefox profile's certificate database, through `certutil` from the NSS tools
    /// (`brew install nss`, or `libnss3-tools` / `nss-tools` on Linux).
    #[schemars(extend("examples" = ["firefox"]))]
    Firefox,
    /// The store Chrome and Chromium read: `~/.pki/nssdb` through `certutil` on Linux, the
    /// same login keychain as `system` on macOS.
    #[schemars(extend("examples" = ["chrome"]))]
    Chrome,
    /// The Windows user root store, read by Edge, Chrome and Firefox on Windows. For
    /// devlanes inside WSL, whose browser usually runs on Windows: it runs Windows'
    /// `certutil.exe` through interop, and Windows asks you to confirm each change.
    /// On Windows it is the same store as `system`; on macOS and Linux outside WSL
    /// `certs trust` reports it as unavailable.
    #[schemars(extend("examples" = ["windows"]))]
    Windows,
    /// Reserved for the iOS simulator; not supported yet, so it is dropped with a warning.
    #[schemars(extend("examples" = ["ios-simulator"]))]
    IosSimulator,
    /// Reserved for the Android emulator; not supported yet, so it is dropped with a warning.
    #[schemars(extend("examples" = ["android-emulator"]))]
    AndroidEmulator,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Ports and network exposure of the daemon's proxy, which routes each lane hostname to
/// its service.
#[schemars(extend("examples" = [{"https_port":443,"http_port":80,"lan":false,"via":true}]))]
pub struct ProxyConfig {
    /// Port the HTTPS proxy listens on; defaults to 443. When the port is taken or not
    /// allowed (low ports on Linux need extra permission), the daemon falls back to 8443
    /// and service URLs carry the port.
    #[schemars(extend("examples" = [443,8443]))]
    pub https_port: u16,
    /// Port for plain HTTP; defaults to 80, falling back to 8080 like `https_port`, and must
    /// differ from it. While HTTPS runs it redirects known hostnames to HTTPS; without a
    /// CA it serves them over plain HTTP.
    #[schemars(extend("examples" = [80,8080]))]
    pub http_port: u16,
    /// Accept connections from other devices on the network; defaults to false, which
    /// refuses every peer that is not this machine. Those devices must still resolve the
    /// hostnames to this machine and trust the CA.
    #[schemars(extend("examples" = [false,true]))]
    pub lan: bool,
    /// Add `Via: 1.1 devlanes.run` to every response the proxy forwards from a service
    /// (`1.1 devlanes.run (remote)` for lanes on a remote box), so a client can tell that
    /// devlanes sat in between. Defaults to true; set false if a client or test compares
    /// response headers exactly.
    #[schemars(extend("examples" = [true,false]))]
    pub via: bool,
}

/// The port the HTTPS proxy binds when [`ProxyConfig::https_port`] is taken or not allowed.
pub const HTTPS_FALLBACK_PORT: u16 = 8443;

/// The port the HTTP proxy binds when [`ProxyConfig::http_port`] is taken or not allowed.
pub const HTTP_FALLBACK_PORT: u16 = 8080;

impl Default for ProxyConfig {
    /// Prefer standard ports; runtime binding handles permission-related fallbacks.
    fn default() -> Self {
        Self {
            https_port: 443,
            http_port: 80,
            lan: false,
            via: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Retention of the service output stored under `~/.devlanes/logs`.
#[schemars(extend("examples" = [{"retention":{"days":7,"max_size":"5GiB"}}]))]
pub struct LogsConfig {
    /// Age and total-size limits for stored service output; a cleanup every 10 minutes
    /// deletes the oldest output past either one. Defaults to 7 days and `5GiB`.
    pub retention: LogRetention,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
/// Limits for stored service output; reaching either one deletes the oldest output first.
#[schemars(extend("examples" = [{"days":7,"max_size":"5GiB"}]))]
pub struct LogRetention {
    /// Delete output older than this many days, along with its index entries in the
    /// database; defaults to 7 and must be at least 1. The newest log file of each
    /// stopped service is kept.
    #[schemars(range(min = 1), extend("examples" = [7]))]
    pub days: u32,
    /// Total size of stored output across all projects and lanes; defaults to `5GiB`
    /// and must be above zero. Over it, the oldest output goes first wherever it is, but
    /// each stopped service's newest file and up to 1 MiB of a running service's current
    /// file are kept.
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
/// Retention of the CPU and memory history in the daemon's database.
#[schemars(extend("examples" = [{"retention":"7d"}]))]
pub struct MetricsConfig {
    /// How long per-minute CPU and memory samples, per-run summaries and startup memory
    /// peaks are kept; defaults to `7d` and must be longer than `0s`. Shorter keeps less
    /// history for graphs and for the memory a start is expected to need;
    /// `state.max_size` can delete samples sooner.
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

/// Terminal integration of the CLI, or a terminal app name from older versions, which is
/// ignored.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = [{"progress":"auto"}]))]
pub enum TerminalConfig {
    /// A terminal app name from when devlanes opened units in a new terminal
    /// window; that feature is gone, so the name is ignored with a warning.
    #[schemars(extend("examples" = ["ghostty"]))]
    Legacy(String),
    /// Terminal settings written as a mapping.
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
    /// Whether starts show their progress in the terminal tab's own progress bar (the
    /// OSC 9;4 sequence); defaults to `auto`. Only human output on a terminal gets it, and
    /// `DEVLANES_TERM_PROGRESS=0` or `1` overrides this setting.
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

/// When start progress goes to the terminal tab's progress bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(extend("examples" = ["auto","always","never"]))]
pub enum TerminalProgress {
    /// Only in terminals known to draw it (Ghostty, WezTerm, iTerm2 3.6.6+, Windows
    /// Terminal, ConEmu, Ptyxis 48+, Konsole 26.04+); never inside tmux, screen, CI or a
    /// dumb terminal.
    #[default]
    #[schemars(extend("examples" = ["auto"]))]
    Auto,
    /// In every terminal, for ones `auto` does not recognize such as kitty; a terminal
    /// without support may ignore it or show it as text or a notification. Inside tmux it
    /// is passed through, which needs `allow-passthrough on`.
    #[schemars(extend("examples" = ["always"]))]
    Always,
    /// Never draw the progress bar.
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
    /// Reject the deferred public resolver, invalid machine-wide resource ranges, a
    /// `hostnames.tld` that could name a real site and retention limits that would
    /// delete everything. Messages start with the offending field path.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.state.max_size.0 == 0 {
            return Err(ConfigError::plain(
                "state.max_size: must be greater than zero",
            ));
        }
        self.validate_retention()?;
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
        if let Some(tld) = &self.hostnames.tld
            && let Err(reason) = check_tld(tld)
        {
            return Err(ConfigError::plain(format!(
                "hostnames.tld: `{tld}` cannot be used: {reason}"
            )));
        }
        self.startup.validate()?;
        self.lanes.validate()?;
        Ok(())
    }

    /// Zero limits would delete all stored output or history at the next cleanup
    /// instead of keeping some; the daemon gives zero no other meaning.
    fn validate_retention(&self) -> Result<(), ConfigError> {
        if self.logs.retention.days == 0 {
            return Err(ConfigError::plain(
                "logs.retention.days: must be at least 1; 0 would delete all stored output",
            ));
        }
        if self.logs.retention.max_size.0 == 0 {
            return Err(ConfigError::plain(
                "logs.retention.max_size: must be greater than zero; 0B would delete all stored output",
            ));
        }
        for (path, retention) in [
            ("history.retention", &self.history.retention),
            (
                "history.remote_hosts_retention",
                &self.history.remote_hosts_retention,
            ),
            ("metrics.retention", &self.metrics.retention),
        ] {
            if retention.0.is_zero() {
                return Err(ConfigError::plain(format!(
                    "{path}: must be longer than 0s; 0s would delete every record at the next cleanup"
                )));
            }
        }
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

/// Special-use zones `hostnames.tld` may equal or sit under, as in
/// `devlanes_resolver_dns::RESERVED_ZONES`.
const RESERVED_TLDS: &[&str] = &["test", "internal", "home.arpa"];

/// Why `tld` cannot be used, by the rule of `devlanes_resolver_dns::Zone::new`
/// (`crates/resolver-dns/src/zone.rs`). Duplicated because config must not depend on
/// the DNS server crate and its runtime; a test in `bins/devlanes` keeps the two in step.
fn check_tld(tld: &str) -> Result<(), &'static str> {
    let name = tld.strip_suffix('.').unwrap_or(tld).to_ascii_lowercase();
    if name.is_empty() {
        return Err("it is empty");
    }
    if name.len() > 253 {
        return Err("it is longer than 253 characters");
    }
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err("every label must be 1 to 63 characters");
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || label.starts_with('-')
            || label.ends_with('-')
        {
            return Err("labels may only hold letters, digits and inner hyphens");
        }
    }
    if name
        .rsplit('.')
        .next()
        .is_some_and(|last| last.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("the last label cannot be numeric");
    }
    let under = |suffix: &str| {
        name.strip_suffix(suffix)
            .is_some_and(|rest| rest.ends_with('.'))
    };
    if RESERVED_TLDS
        .iter()
        .any(|suffix| name == *suffix || under(suffix))
        || under("localhost")
    {
        Ok(())
    } else {
        Err(
            "the TLD must be test, internal or home.arpa, a domain under one of them, \
             or a domain under localhost",
        )
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
