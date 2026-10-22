//! `doctor`'s `excluded_ports` on Windows: the TCP port ranges Windows keeps out
//! of reach (Hyper-V, WSL, Docker Desktop and administrators reserve them),
//! checked against the port pool and the ports mirrored from connected boxes.
//! Binding inside one fails with access denied, so a service given such a port
//! cannot start and a mirror cannot listen there.

use std::{collections::BTreeSet, process::Stdio, time::Duration};

use devlanes_sys::{NoWindow, windows::net::parse_excluded_ranges};
use serde_json::{Value, json};

/// `netsh` answers within a second; a hung one must not hold up `doctor`.
const NETSH_TIMEOUT: Duration = Duration::from_secs(10);

/// The excluded TCP ranges of both address families, sorted and without
/// duplicates, or why they could not be read.
pub async fn read() -> Result<Vec<(u16, u16)>, String> {
    let mut ranges = Vec::new();
    for family in ["ipv4", "ipv6"] {
        let output = tokio::process::Command::new("netsh")
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
        let output = match tokio::time::timeout(NETSH_TIMEOUT, output).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(format!("cannot run netsh: {error}")),
            Err(_) => {
                return Err(format!(
                    "netsh did not answer within {}s",
                    NETSH_TIMEOUT.as_secs()
                ));
            }
        };
        if !output.status.success() {
            return Err(format!("netsh failed ({})", output.status));
        }
        // The output is in the console code page; only the ASCII digits matter.
        ranges.extend(parse_excluded_ranges(&String::from_utf8_lossy(
            &output.stdout,
        )));
    }
    ranges.sort_unstable();
    ranges.dedup();
    Ok(ranges)
}

/// `{ranges, pool, pool_overlaps, mirrored}` from `ranges`, or `{error}`.
/// `pool` is the inclusive `ports.range` in effect (`None` when it is invalid,
/// which the user config check reports); `mirrored` are the ports of connected
/// boxes, which the mirror binds under the same number. `pool_overlaps` lists the
/// ranges that overlap the pool and `mirrored` the box ports inside a range.
pub fn report(
    ranges: Result<Vec<(u16, u16)>, String>,
    pool: Option<(u16, u16)>,
    mirrored: &BTreeSet<u16>,
) -> Value {
    let ranges = match ranges {
        Ok(ranges) => ranges,
        Err(error) => return json!({"error": error}),
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
        .filter(|port| {
            ranges
                .iter()
                .any(|(start, end)| (*start..=*end).contains(port))
        })
        .collect();
    json!({
        "ranges": ranges.iter().map(range).collect::<Vec<_>>(),
        "pool": pool.map(|pool| range(&pool)),
        "pool_overlaps": pool_overlaps,
        "mirrored": blocked,
    })
}

/// [`read`] then [`report`].
pub async fn doctor(pool: Option<(u16, u16)>, mirrored: &BTreeSet<u16>) -> Value {
    report(read().await, pool, mirrored)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Overlaps with the pool are whole ranges, touching either end counts, and
    /// only box ports inside a range are reported.
    #[test]
    fn overlaps_with_the_pool_and_mirrored_ports() {
        let ranges = vec![(1000, 1999), (27339, 27339), (32767, 33000), (50000, 50059)];
        let mirrored = BTreeSet::from([27339, 27340, 50059]);
        let report = report(Ok(ranges), Some((20000, 32767)), &mirrored);
        assert_eq!(
            report["pool_overlaps"],
            json!([{"start": 27339, "end": 27339}, {"start": 32767, "end": 33000}])
        );
        assert_eq!(report["mirrored"], json!([27339, 50059]));
        assert_eq!(report["pool"], json!({"start": 20000, "end": 32767}));
        assert_eq!(report["ranges"].as_array().map(Vec::len), Some(4));
    }

    /// Without a valid pool only the mirrored ports are judged; a failed read is
    /// an error alone.
    #[test]
    fn no_pool_and_failed_reads() {
        let report = report(Ok(vec![(20000, 20010)]), None, &BTreeSet::new());
        assert_eq!(report["pool"], Value::Null);
        assert_eq!(report["pool_overlaps"], json!([]));
        let failed = super::report(Err("netsh failed".into()), Some((1, 2)), &BTreeSet::new());
        assert_eq!(failed, json!({"error": "netsh failed"}));
    }

    /// The real `netsh` answers and its rows parse: every range is ordered.
    #[tokio::test]
    async fn netsh_ranges_are_read() {
        let ranges = read().await.expect("netsh output");
        assert!(ranges.iter().all(|(start, end)| start <= end), "{ranges:?}");
    }
}
