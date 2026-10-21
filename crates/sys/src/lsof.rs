//! Parser for `lsof -F ptn` output; compiled on every platform for unit tests.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::ListeningSocket;

/// Parses field-mode output: `p<pid>` starts a process, `f` a file, `t` the address
/// family and `n` the name, which comes last within each file.
pub(crate) fn parse(output: &str) -> Vec<ListeningSocket> {
    let mut sockets = Vec::new();
    let mut pid = None;
    let mut ipv6 = false;
    for line in output.lines() {
        let (Some(tag), Some(value)) = (line.get(..1), line.get(1..)) else {
            continue;
        };
        match tag {
            "p" => pid = value.parse().ok(),
            "f" => ipv6 = false,
            "t" => ipv6 = value == "IPv6",
            "n" => {
                if let (Some(pid), Some(addr)) = (pid, parse_name(value, ipv6)) {
                    sockets.push(ListeningSocket { pid, addr });
                }
            }
            _ => {}
        }
    }
    sockets
}

/// Parses `127.0.0.1:80`, `[::1]:80` or `*:80`; a wildcard host binds every address.
fn parse_name(name: &str, ipv6: bool) -> Option<SocketAddr> {
    let (host, port) = name.rsplit_once(':')?;
    let port = port.parse().ok()?;
    let ip = match host {
        "*" if ipv6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        "*" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        _ => host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host)
            .parse()
            .ok()?,
    };
    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every address form lsof prints for listeners is attributed to its process.
    #[test]
    fn listeners() {
        let output = "p10\nf5\ntIPv4\nn127.0.0.1:4999\nf6\ntIPv6\nn[::1]:4999\np11\nf3\ntIPv6\nn*:80\nf4\ntIPv4\nn*:81\n";
        let addrs: Vec<(u32, String)> = parse(output)
            .into_iter()
            .map(|s| (s.pid, s.addr.to_string()))
            .collect();
        assert_eq!(
            addrs,
            [
                (10, "127.0.0.1:4999".into()),
                (10, "[::1]:4999".into()),
                (11, "[::]:80".into()),
                (11, "0.0.0.0:81".into()),
            ]
        );
    }
}
