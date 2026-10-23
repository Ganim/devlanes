//! The TCP port ranges Windows keeps out of reach: Hyper-V, WSL, Docker
//! Desktop and administrators reserve them, and binding inside one fails with
//! access denied. The port allocator keeps new blocks clear of them, and
//! `doctor` (the daemon's and the CLI's own) reports which of them hold lane
//! ports assigned before the range appeared, the proxy ports or mirrored ports.

use std::collections::BTreeSet;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::Mutex;

/// The command that lists the IPv4 ranges, for hints.
pub const LIST_COMMAND: &str = "netsh int ipv4 show excludedportrange protocol=tcp";

/// `netsh` answers within a second; a hung one must not hold up `doctor` or a start.
const NETSH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long [`Cache`] reuses a reading. The TUI asks for `doctor` every few
/// seconds and every lane start allocates, while the ranges only change when
/// Hyper-V, WSL or Docker start, an administrator edits them, or Windows boots.
const CACHE_FOR: Duration = Duration::from_secs(60);

/// How long [`Cache`] reuses a reading where no family could be read, so a
/// passing `netsh` failure does not leave the allocator blind for a minute
/// while the TUI's `doctor` polls still share one run.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// The address families `netsh` lists separately.
const FAMILIES: [&str; 2] = ["ipv4", "ipv6"];

/// The reserved ranges of the families that could be read, as inclusive
/// `(start, end)` pairs, sorted and without duplicates, and why the others
/// could not be read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reading {
    pub ranges: Vec<(u16, u16)>,
    /// `family: reason` for each family that failed.
    pub errors: Vec<String>,
}

impl Reading {
    /// Whether at least one family was read, so `ranges` means something.
    pub fn any_read(&self) -> bool {
        self.errors.len() < FAMILIES.len()
    }

    /// Whether `port` lies in a range.
    pub fn reserves(&self, port: u16) -> bool {
        self.ranges
            .iter()
            .any(|(start, end)| (*start..=*end).contains(&port))
    }

    /// The reserved ports between `first` and `last`, both included, for the
    /// allocator's `avoid` set.
    pub fn ports_within(&self, first: u16, last: u16) -> BTreeSet<u16> {
        self.ranges
            .iter()
            .filter_map(|&(start, end)| {
                let (start, end) = (start.max(first), end.min(last));
                (start <= end).then_some(start..=end)
            })
            .flatten()
            .collect()
    }
}

/// The ranges in `netsh interface <family> show excludedportrange
/// protocol=tcp` output. Rows are two port numbers, the second followed by
/// `*` for administered exclusions. Titles and headers are translated on
/// localized Windows, so every line that does not start with two port numbers
/// is skipped, and so is a pair whose start lies after its end.
fn parse(text: &str) -> Vec<(u16, u16)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let start: u16 = fields.next()?.parse().ok()?;
            let end: u16 = fields.next()?.parse().ok()?;
            (start <= end).then_some((start, end))
        })
        .collect()
}

/// Reads both families concurrently; one that fails leaves the other's ranges.
async fn read() -> Reading {
    let (ipv4, ipv6) = tokio::join!(read_family(FAMILIES[0]), read_family(FAMILIES[1]));
    let mut reading = Reading::default();
    for (family, result) in FAMILIES.into_iter().zip([ipv4, ipv6]) {
        match result {
            Ok(ranges) => reading.ranges.extend(ranges),
            Err(error) => reading.errors.push(format!("{family}: {error}")),
        }
    }
    reading.ranges.sort_unstable();
    reading.ranges.dedup();
    reading
}

/// One family's ranges from `<System32>\netsh.exe`, never a `netsh` on `PATH`.
async fn read_family(family: &str) -> Result<Vec<(u16, u16)>, String> {
    let netsh = devlanes_sys_windows::file::system_directory()
        .map_err(|error| format!("cannot locate System32: {error}"))?
        .join("netsh.exe");
    let output = tokio::process::Command::new(netsh)
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
        .map_err(|_| format!("netsh did not answer within {}s", NETSH_TIMEOUT.as_secs()))?
        .map_err(|error| format!("cannot run netsh: {error}"))?;
    if !output.status.success() {
        return Err(format!("netsh failed ({})", output.status));
    }
    // The output is in the console code page; only the ASCII digits matter.
    Ok(parse(&String::from_utf8_lossy(&output.stdout)))
}

/// The last [`Reading`] and when it was taken. Callers at the same time share
/// one `netsh` run.
pub struct Cache {
    last: Mutex<Option<(Instant, Arc<Reading>)>>,
}

impl Cache {
    /// An empty cache; the first [`Cache::get`] reads.
    const fn new() -> Self {
        Self {
            last: Mutex::const_new(None),
        }
    }

    /// The last reading while it is younger than a minute (five seconds when
    /// no family was read), else a new one.
    pub async fn get(&self) -> Arc<Reading> {
        let mut last = self.last.lock().await;
        if let Some((at, reading)) = last.as_ref()
            && at.elapsed() < fresh_for(reading)
        {
            return reading.clone();
        }
        let reading = Arc::new(read().await);
        *last = Some((Instant::now(), reading.clone()));
        reading
    }
}

/// How long [`Cache`] keeps `reading`.
fn fresh_for(reading: &Reading) -> Duration {
    if reading.any_read() {
        CACHE_FOR
    } else {
        RETRY_AFTER
    }
}

/// The process-wide cache, shared by `doctor` and the port allocator.
pub static RESERVED: Cache = Cache::new();

/// The fallback ports the proxy binds when its configured ones fail.
const PROXY_FALLBACKS: [(&str, u16); 2] = [
    ("http", devlanes_config::HTTP_FALLBACK_PORT),
    ("https", devlanes_config::HTTPS_FALLBACK_PORT),
];

/// `doctor`'s `reserved_ports`: `{ranges, pool, pool_overlaps, lanes, proxy,
/// mirrored, error?, command}`, or `{error, command}` when no family could be
/// read. `pool` is the inclusive `ports.range` (`None` when invalid, which the
/// user config check reports); `pool_overlaps` are the ranges overlapping it,
/// which new blocks skip; `lanes` the lanes whose assigned ports fell into a
/// range since (`[{lane, ports}]`, from `assigned`: lane reference to ports);
/// `proxy` the proxy ports inside a range, configured or fallback
/// (`[{listener, port, fallback}]`); `mirrored` the mirrored ports inside one.
pub fn report(
    reading: &Reading,
    pool: Option<(u16, u16)>,
    assigned: &[(String, BTreeSet<u16>)],
    proxy: [(&'static str, u16); 2],
    mirrored: &BTreeSet<u16>,
) -> Value {
    let error = (!reading.errors.is_empty()).then(|| reading.errors.join("; "));
    if !reading.any_read() {
        return json!({"error": error, "command": LIST_COMMAND});
    }
    let range = |&(start, end): &(u16, u16)| json!({"start": start, "end": end});
    let pool_overlaps: Vec<Value> = pool
        .map(|(first, last)| {
            reading
                .ranges
                .iter()
                .filter(|(start, end)| *start <= last && first <= *end)
                .map(range)
                .collect()
        })
        .unwrap_or_default();
    let lanes: Vec<Value> = assigned
        .iter()
        .filter_map(|(lane, ports)| {
            let blocked: Vec<u16> = ports
                .iter()
                .copied()
                .filter(|port| reading.reserves(*port))
                .collect();
            (!blocked.is_empty()).then(|| json!({"lane": lane, "ports": blocked}))
        })
        .collect();
    let mut proxy_ports: Vec<(&str, u16, bool)> = Vec::new();
    for (listener, port) in proxy {
        proxy_ports.push((listener, port, false));
        if let Some((_, fallback)) = PROXY_FALLBACKS
            .iter()
            .find(|(name, fallback)| *name == listener && *fallback != port)
        {
            proxy_ports.push((listener, *fallback, true));
        }
    }
    let proxy: Vec<Value> = proxy_ports
        .into_iter()
        .filter(|(_, port, _)| reading.reserves(*port))
        .map(|(listener, port, fallback)| {
            json!({"listener": listener, "port": port, "fallback": fallback})
        })
        .collect();
    let mirrored: Vec<u16> = mirrored
        .iter()
        .copied()
        .filter(|port| reading.reserves(*port))
        .collect();
    json!({
        "ranges": reading.ranges.iter().map(range).collect::<Vec<_>>(),
        "pool": pool.map(|pool| range(&pool)),
        "pool_overlaps": pool_overlaps,
        "lanes": lanes,
        "proxy": proxy,
        "mirrored": mirrored,
        "error": error,
        "command": LIST_COMMAND,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `netsh int ipv4 show excludedportrange protocol=tcp` on English Windows 11.
    const ENGLISH: &str = "\r\nProtocol tcp Port Exclusion Ranges\r\n\r\nStart Port    End Port      \r\n----------    --------      \r\n     27339       27339      \r\n     49745       49844      \r\n     50000       50059     *\r\n\r\n* - Administered port exclusions.\r\n\r\n";

    /// The same with German headers, as on a localized installation.
    const GERMAN: &str = "\r\nProtokoll tcp Portausschlussbereiche\r\n\r\nStartport    Endport\r\n----------    --------\r\n      5357        5357\r\n     50000       50059     *\r\n\r\n* - Verwaltete Portausschl\u{fc}sse.\r\n";

    /// Rows are read whatever the language; headers, footers and nonsense rows
    /// are not.
    #[test]
    fn parses_numeric_rows_only() {
        assert_eq!(
            parse(ENGLISH),
            [(27339, 27339), (49745, 49844), (50000, 50059)]
        );
        assert_eq!(parse(GERMAN), [(5357, 5357), (50000, 50059)]);
        assert!(parse("").is_empty());
        assert!(parse("  70000 70001\n 9 8\n 12\n").is_empty());
    }

    /// Only the reserved ports inside the pool are avoided.
    #[test]
    fn ports_within_the_pool() {
        let reading = read_ok(vec![(19990, 20001), (27339, 27339), (32767, 33000)]);
        let ports = reading.ports_within(20000, 32767);
        assert_eq!(
            ports.into_iter().collect::<Vec<_>>(),
            [20000, 20001, 27339, 32767]
        );
        assert!(reading.ports_within(40000, 50000).is_empty());
    }

    /// A reading where both families failed is kept for seconds, one with any
    /// family read for the full minute.
    #[test]
    fn failed_readings_are_retried_soon() {
        let failed = Reading {
            ranges: Vec::new(),
            errors: vec!["ipv4: x".into(), "ipv6: y".into()],
        };
        assert_eq!(fresh_for(&failed), RETRY_AFTER);
        let partial = Reading {
            ranges: Vec::new(),
            errors: vec!["ipv6: y".into()],
        };
        assert_eq!(fresh_for(&partial), CACHE_FOR);
        assert_eq!(fresh_for(&Reading::default()), CACHE_FOR);
    }

    /// The real `netsh` answers for both families with ordered ranges, and the
    /// cache hands out the same reading while it is fresh.
    #[tokio::test]
    async fn netsh_is_read_and_cached() {
        let cache = Cache::new();
        let reading = cache.get().await;
        assert!(reading.errors.is_empty(), "{:?}", reading.errors);
        assert!(reading.ranges.iter().all(|(start, end)| start <= end));
        assert!(Arc::ptr_eq(&reading, &cache.get().await));
    }

    /// A reading with `ranges` and every family read.
    fn read_ok(ranges: Vec<(u16, u16)>) -> Reading {
        Reading {
            ranges,
            errors: Vec::new(),
        }
    }

    /// Ranges touching either end of the pool overlap it; only lane, mirrored
    /// and proxy ports inside a range are reported, fallbacks included.
    #[test]
    fn report_lists_overlaps() {
        let reading = read_ok(vec![(80, 80), (8440, 8449), (27339, 27339), (32767, 33000)]);
        let report = report(
            &reading,
            Some((20000, 32767)),
            &[
                ("acme@0".into(), BTreeSet::from([20000, 20001])),
                ("acme@1".into(), BTreeSet::from([27338, 27339])),
            ],
            [("http", 80), ("https", 443)],
            &BTreeSet::from([27339, 27340]),
        );
        assert_eq!(
            report["pool_overlaps"],
            json!([{"start": 27339, "end": 27339}, {"start": 32767, "end": 33000}])
        );
        assert_eq!(
            report["lanes"],
            json!([{"lane": "acme@1", "ports": [27339]}])
        );
        assert_eq!(report["mirrored"], json!([27339]));
        assert_eq!(
            report["proxy"],
            json!([
                {"listener": "http", "port": 80, "fallback": false},
                {"listener": "https", "port": 8443, "fallback": true},
            ])
        );
        assert_eq!(report["error"], Value::Null);
        assert_eq!(report["command"], LIST_COMMAND);

        // A configured port equal to its fallback is listed once.
        let report = super::report(
            &reading,
            None,
            &[],
            [("http", 8080), ("https", 8443)],
            &BTreeSet::new(),
        );
        assert_eq!(
            report["proxy"],
            json!([{"listener": "https", "port": 8443, "fallback": false}])
        );
        assert_eq!(report["pool"], Value::Null);
        assert_eq!(report["pool_overlaps"], json!([]));
    }

    /// One failed family is reported beside the other's ranges; with none read
    /// only the error is left.
    #[test]
    fn failed_reads_are_reported() {
        let partial = Reading {
            ranges: vec![(20000, 20010)],
            errors: vec!["ipv6: netsh failed".into()],
        };
        let report = report(
            &partial,
            Some((20000, 20005)),
            &[],
            [("http", 80), ("https", 443)],
            &BTreeSet::new(),
        );
        assert_eq!(report["error"], "ipv6: netsh failed");
        assert_eq!(
            report["pool_overlaps"],
            json!([{"start": 20000, "end": 20010}])
        );
        let failed = Reading {
            ranges: Vec::new(),
            errors: vec!["ipv4: x".into(), "ipv6: y".into()],
        };
        assert_eq!(
            super::report(
                &failed,
                None,
                &[],
                [("http", 80), ("https", 443)],
                &BTreeSet::new()
            ),
            json!({"error": "ipv4: x; ipv6: y", "command": LIST_COMMAND})
        );
    }
}
