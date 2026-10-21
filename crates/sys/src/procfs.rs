//! Pure parsers for Linux procfs text; compiled on every platform for unit tests.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Fields of `/proc/<pid>/stat` the supervisor needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stat {
    pub pid: u32,
    pub name: String,
    pub state: char,
    pub ppid: u32,
    pub pgid: u32,
    pub sid: u32,
    /// Field 22, clock ticks after boot.
    pub start_time: u64,
}

/// Parses one stat line. The command name is located by the last `)` because it
/// may itself contain spaces and parentheses.
pub(crate) fn parse_stat(line: &str) -> Option<Stat> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let pid = line.get(..open)?.trim().parse().ok()?;
    let name = line.get(open + 1..close)?.to_owned();
    let mut fields = line.get(close + 1..)?.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    let pgid = fields.next()?.parse().ok()?;
    let sid = fields.next()?.parse().ok()?;
    // Fields 3 to 6 are consumed; the iterator now starts at field 7.
    let start_time = fields.nth(22 - 7)?.parse().ok()?;
    Some(Stat {
        pid,
        name,
        state,
        ppid,
        pgid,
        sid,
        start_time,
    })
}

/// One row of `/proc/net/tcp` or `/proc/net/tcp6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TcpRow {
    pub local: SocketAddr,
    pub listening: bool,
    pub inode: u64,
}

/// Parses a TCP table; malformed rows are skipped because the header and
/// trailing fields vary between kernel versions.
pub(crate) fn parse_tcp_table(content: &str, ipv6: bool) -> Vec<TcpRow> {
    content
        .lines()
        .skip(1)
        .filter_map(|line| {
            let columns: Vec<&str> = line.split_ascii_whitespace().collect();
            let (ip, port) = columns.get(1)?.split_once(':')?;
            let ip = if ipv6 {
                parse_ipv6(ip)?
            } else {
                parse_ipv4(ip)?
            };
            Some(TcpRow {
                local: SocketAddr::new(ip, u16::from_str_radix(port, 16).ok()?),
                // 0A is TCP_LISTEN in the kernel's state enumeration.
                listening: *columns.get(3)? == "0A",
                inode: columns.get(9)?.parse().ok()?,
            })
        })
        .collect()
}

/// Parses the socket inode from an fd symlink target such as `socket:[1234]`.
pub(crate) fn socket_inode(target: &str) -> Option<u64> {
    target
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Decodes an address the kernel printed as host-endian 32-bit words of network-order bytes.
fn parse_ipv4(hex: &str) -> Option<IpAddr> {
    let word = u32::from_str_radix(hex, 16).ok()?;
    Some(IpAddr::V4(Ipv4Addr::from(word.to_ne_bytes())))
}

/// Decodes the four host-endian words of an IPv6 address.
fn parse_ipv6(hex: &str) -> Option<IpAddr> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0_u8; 16];
    for (index, chunk) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let word = u32::from_str_radix(hex.get(index * 8..index * 8 + 8)?, 16).ok()?;
        *chunk = word.to_ne_bytes();
    }
    Some(IpAddr::V6(Ipv6Addr::from(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names with spaces and parentheses do not shift the numeric fields.
    #[test]
    fn stat_with_awkward_name() {
        let line = "4242 (my (odd) name) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10 18446744073709551615";
        let stat = parse_stat(line).expect("valid stat line");
        assert_eq!(
            stat,
            Stat {
                pid: 4242,
                name: "my (odd) name".into(),
                state: 'S',
                ppid: 1,
                pgid: 4242,
                sid: 4242,
                start_time: 987654,
            }
        );
        assert_eq!(parse_stat("4242 (truncated) S 1"), None);
    }

    /// Listening rows decode little-endian kernel output for both families.
    #[cfg(target_endian = "little")]
    #[test]
    fn tcp_tables() {
        let v4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 55501 1 0000000000000000 100 0 0 10 0\n   1: 0100007F:A3C2 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 55502 1\n";
        let rows = parse_tcp_table(v4, false);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].local, "127.0.0.1:8080".parse().expect("addr"));
        assert!(rows[0].listening);
        assert_eq!(rows[0].inode, 55501);
        assert!(!rows[1].listening);

        let v6 = "header\n   0: 00000000000000000000000001000000:0FA0 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 77 1\n";
        let rows = parse_tcp_table(v6, true);
        assert_eq!(rows[0].local, "[::1]:4000".parse().expect("addr"));
        assert_eq!(socket_inode("socket:[77]"), Some(77));
        assert_eq!(socket_inode("pipe:[77]"), None);
    }
}
