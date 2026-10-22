//! The TCP port ranges Windows keeps out of reach (Hyper-V, WSL, Docker Desktop
//! and administrators reserve them). Binding inside one fails with access
//! denied, so the port allocator skips them and `doctor`'s `excluded_ports`
//! checks them against the port pool, the proxy ports and the ports mirrored from
//! connected boxes.

use std::{
    collections::BTreeSet,
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use devlanes_config::ProxyConfig;
use devlanes_core::ports::PortBlock;
use devlanes_sys::{
    NoWindow,
    windows::{dirs::system_directory, net::parse_excluded_ranges},
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// `netsh` answers within a second; a hung one must not hold up `doctor`.
const NETSH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long [`Cache`] reuses a reading. The TUI asks for `doctor` every 3 s and
/// every lane start allocates, while the ranges only change when Hyper-V, WSL or
/// Docker start, an administrator edits them, or Windows reboots.
const CACHE_FOR: Duration = Duration::from_secs(60);

/// The address families `netsh` lists separately.
const FAMILIES: [&str; 2] = ["ipv4", "ipv6"];

/// Why one family's ranges could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// System32, where `netsh.exe` lives, could not be located.
    #[error("cannot locate System32: {0}")]
    SystemDirectory(#[source] std::io::Error),
    /// `netsh` could not be started.
    #[error("cannot run netsh: {0}")]
    Spawn(#[source] std::io::Error),
    /// `netsh` did not exit in time.
    #[error("netsh did not answer within {}s", NETSH_TIMEOUT.as_secs())]
    Timeout,
    /// `netsh` exited unsuccessfully.
    #[error("netsh failed ({0})")]
    Failed(ExitStatus),
}

/// The excluded TCP ranges of the families that could be read, as inclusive
/// `(start, end)` pairs, sorted and without duplicates, plus the families that
/// could not.
#[derive(Debug, Default)]
pub struct Reading {
    pub ranges: Vec<(u16, u16)>,
    pub errors: Vec<(&'static str, ReadError)>,
}

impl Reading {
    /// Whether at least one family was read, so `ranges` means something.
    pub fn any_read(&self) -> bool {
        self.errors.len() < FAMILIES.len()
    }

    /// The failed families as one line, `None` when every family was read.
    pub fn error(&self) -> Option<String> {
        (!self.errors.is_empty()).then(|| {
            self.errors
                .iter()
                .map(|(family, error)| format!("{family}: {error}"))
                .collect::<Vec<_>>()
                .join("; ")
        })
    }

    /// The ranges as port blocks, for the allocator to keep new blocks clear of.
    pub fn blocks(&self) -> Vec<PortBlock> {
        self.ranges
            .iter()
            .filter_map(|&(start, end)| {
                let size = u16::try_from(u32::from(end) - u32::from(start) + 1).ok()?;
                PortBlock::new(start, size).ok()
            })
            .collect()
    }
}

/// Reads both families concurrently.
pub async fn read() -> Reading {
    let (ipv4, ipv6) = tokio::join!(read_family(FAMILIES[0]), read_family(FAMILIES[1]));
    let mut reading = Reading::default();
    for (family, result) in FAMILIES.into_iter().zip([ipv4, ipv6]) {
        match result {
            Ok(ranges) => reading.ranges.extend(ranges),
            Err(error) => reading.errors.push((family, error)),
        }
    }
    reading.ranges.sort_unstable();
    reading.ranges.dedup();
    reading
}

/// `netsh interface <family> show excludedportrange protocol=tcp`, run from
/// System32 by full path so a `netsh` earlier on `PATH` is never picked.
async fn read_family(family: &'static str) -> Result<Vec<(u16, u16)>, ReadError> {
    let netsh = system_directory()
        .map_err(ReadError::SystemDirectory)?
        .join("netsh.exe");
    let output = tokio::process::Command::new(netsh)
        .no_window()
        .args([
            "interface",
            family,
            "show",
            "excludedportrange",
            "protocol=tcp",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(NETSH_TIMEOUT, output)
        .await
        .map_err(|_| ReadError::Timeout)?
        .map_err(ReadError::Spawn)?;
    if !output.status.success() {
        return Err(ReadError::Failed(output.status));
    }
    // The output is in the console code page; only the ASCII digits matter.
    Ok(parse_excluded_ranges(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// The last [`Reading`] and when it was taken. Callers at the same time share one
/// `netsh` run.
pub struct Cache {
    last: Mutex<Option<(Instant, Arc<Reading>)>>,
}

impl Cache {
    /// An empty cache; the first [`Cache::get`] reads.
    pub const fn new() -> Self {
        Self {
            last: Mutex::const_new(None),
        }
    }

    /// The last reading while it is younger than a minute, else a new one.
    pub async fn get(&self) -> Arc<Reading> {
        let mut last = self.last.lock().await;
        if let Some((at, reading)) = last.as_ref()
            && at.elapsed() < CACHE_FOR
        {
            return reading.clone();
        }
        let reading = Arc::new(read().await);
        *last = Some((Instant::now(), reading.clone()));
        reading
    }
}

impl Default for Cache {
    /// Same as [`Cache::new`].
    fn default() -> Self {
        Self::new()
    }
}

/// The daemon's cache, shared by `doctor` and the port allocator.
pub static RESERVED: Cache = Cache::new();

/// The ports the proxy binds as `(listener, port, fallback)`: the configured ones
/// and the fallbacks used when those are denied or taken. A fallback equal to its
/// configured port is not listed twice.
fn proxy_ports(proxy: &ProxyConfig) -> Vec<(&'static str, u16, bool)> {
    let mut ports = Vec::new();
    for (listener, port, fallback) in [
        ("http", proxy.http_port, ProxyConfig::HTTP_FALLBACK_PORT),
        ("https", proxy.https_port, ProxyConfig::HTTPS_FALLBACK_PORT),
    ] {
        ports.push((listener, port, false));
        if fallback != port {
            ports.push((listener, fallback, true));
        }
    }
    ports
}

/// `{ranges, error?, pool, pool_overlaps, mirrored, proxy}` from `reading`, or
/// `{error}` alone when no family could be read. `pool` is the inclusive
/// `ports.range` in effect (`None` when it is invalid, which the user config
/// check reports); `mirrored` are the ports of connected boxes, which the mirror
/// binds under the same number. `pool_overlaps` lists the ranges that overlap the
/// pool, `mirrored` the box ports inside a range and `proxy` the proxy ports
/// inside one (`[{listener, port, fallback}]`).
pub fn report(
    reading: &Reading,
    pool: Option<(u16, u16)>,
    mirrored: &BTreeSet<u16>,
    proxy: &ProxyConfig,
) -> Value {
    if !reading.any_read() {
        return json!({"error": reading.error()});
    }
    let ranges = &reading.ranges;
    let reserved = |port: u16| {
        ranges
            .iter()
            .any(|(start, end)| (*start..=*end).contains(&port))
    };
    let range = |&(start, end): &(u16, u16)| json!({"start": start, "end": end});
    let pool_overlaps: Vec<Value> = pool
        .map(|(first, last)| {
            ranges
                .iter()
                .filter(|(start, end)| *start <= last && first <= *end)
                .map(range)
                .collect()
        })
        .unwrap_or_default();
    let blocked: Vec<u16> = mirrored
        .iter()
        .copied()
        .filter(|port| reserved(*port))
        .collect();
    let proxy: Vec<Value> = proxy_ports(proxy)
        .into_iter()
        .filter(|(_, port, _)| reserved(*port))
        .map(|(listener, port, fallback)| {
            json!({"listener": listener, "port": port, "fallback": fallback})
        })
        .collect();
    let mut report = json!({
        "ranges": ranges.iter().map(range).collect::<Vec<_>>(),
        "pool": pool.map(|pool| range(&pool)),
        "pool_overlaps": pool_overlaps,
        "mirrored": blocked,
        "proxy": proxy,
    });
    if let Some(error) = reading.error() {
        report["error"] = json!(error);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reading with `ranges` and every family read.
    fn read_ok(ranges: Vec<(u16, u16)>) -> Reading {
        Reading {
            ranges,
            errors: Vec::new(),
        }
    }

    /// Overlaps with the pool are whole ranges, touching either end counts, and
    /// only box ports inside a range are reported.
    #[test]
    fn overlaps_with_the_pool_and_mirrored_ports() {
        let reading = read_ok(vec![
            (1000, 1999),
            (27339, 27339),
            (32767, 33000),
            (50000, 50059),
        ]);
        let mirrored = BTreeSet::from([27339, 27340, 50059]);
        let report = report(
            &reading,
            Some((20000, 32767)),
            &mirrored,
            &ProxyConfig::default(),
        );
        assert_eq!(
            report["pool_overlaps"],
            json!([{"start": 27339, "end": 27339}, {"start": 32767, "end": 33000}])
        );
        assert_eq!(report["mirrored"], json!([27339, 50059]));
        assert_eq!(report["pool"], json!({"start": 20000, "end": 32767}));
        assert_eq!(report["ranges"].as_array().map(Vec::len), Some(4));
        assert_eq!(report["proxy"], json!([]));
        assert!(report.get("error").is_none());
    }

    /// Configured proxy ports and the fallbacks inside a range are reported; a
    /// fallback that is the configured port is listed once.
    #[test]
    fn proxy_ports_inside_ranges() {
        let reading = read_ok(vec![(80, 80), (8440, 8449)]);
        let report = report(&reading, None, &BTreeSet::new(), &ProxyConfig::default());
        assert_eq!(
            report["proxy"],
            json!([
                {"listener": "http", "port": 80, "fallback": false},
                {"listener": "https", "port": 8443, "fallback": true},
            ])
        );
        let proxy = ProxyConfig {
            https_port: 8443,
            ..ProxyConfig::default()
        };
        let report = super::report(&reading, None, &BTreeSet::new(), &proxy);
        assert_eq!(
            report["proxy"],
            json!([
                {"listener": "http", "port": 80, "fallback": false},
                {"listener": "https", "port": 8443, "fallback": false},
            ])
        );
    }

    /// Without a valid pool only the other ports are judged; a family that failed
    /// is reported next to the ranges the other gave, and with none read the
    /// error is alone.
    #[test]
    fn no_pool_and_failed_reads() {
        let proxy = ProxyConfig::default();
        let report = report(
            &read_ok(vec![(20000, 20010)]),
            None,
            &BTreeSet::new(),
            &proxy,
        );
        assert_eq!(report["pool"], Value::Null);
        assert_eq!(report["pool_overlaps"], json!([]));

        let partial = Reading {
            ranges: vec![(20000, 20010)],
            errors: vec![("ipv6", ReadError::Timeout)],
        };
        let report = super::report(&partial, Some((20000, 20005)), &BTreeSet::new(), &proxy);
        assert_eq!(report["error"], "ipv6: netsh did not answer within 10s");
        assert_eq!(
            report["pool_overlaps"],
            json!([{"start": 20000, "end": 20010}])
        );

        let failed = Reading {
            ranges: Vec::new(),
            errors: vec![("ipv4", ReadError::Timeout), ("ipv6", ReadError::Timeout)],
        };
        assert_eq!(
            super::report(&failed, Some((1, 2)), &BTreeSet::new(), &proxy),
            json!({"error": "ipv4: netsh did not answer within 10s; ipv6: netsh did not answer within 10s"})
        );
    }

    /// Ranges become port blocks with the same ports; the whole port space still
    /// fits, a range starting at port zero does not.
    #[test]
    fn ranges_become_blocks() {
        let reading = read_ok(vec![(0, 10), (27339, 27339), (50000, 50059), (1, 65535)]);
        assert_eq!(
            reading.blocks(),
            [
                PortBlock {
                    start: 27339,
                    size: 1
                },
                PortBlock {
                    start: 50000,
                    size: 60
                },
                PortBlock {
                    start: 1,
                    size: 65535
                },
            ]
        );
    }

    /// The real `netsh` answers for both families and its rows parse: every range
    /// is ordered. The cache hands out the same reading while it is fresh.
    #[tokio::test]
    async fn netsh_ranges_are_read_and_cached() {
        let cache = Cache::new();
        let reading = cache.get().await;
        assert!(reading.errors.is_empty(), "{:?}", reading.error());
        assert!(
            reading.ranges.iter().all(|(start, end)| start <= end),
            "{:?}",
            reading.ranges
        );
        assert!(Arc::ptr_eq(&reading, &cache.get().await));
    }
}
