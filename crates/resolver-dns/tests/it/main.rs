//! The DNS server over real loopback sockets on an ephemeral port.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use devlanes_resolver_dns::{DnsServer, Zone};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const LIMIT: Duration = Duration::from_secs(5);

fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.') {
        out.push(u8::try_from(label.len()).expect("label"));
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out
}

fn rcode(response: &[u8]) -> u8 {
    response[3] & 0xF
}

fn ancount(response: &[u8]) -> u16 {
    u16::from_be_bytes([response[6], response[7]])
}

/// Starts a server for `test`; the guard cancels it when the test ends.
async fn start() -> (
    SocketAddr,
    tokio_util::sync::DropGuard,
    tokio::task::JoinHandle<()>,
) {
    let listen = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let server = DnsServer::bind(listen, Zone::new("test").expect("zone"))
        .await
        .expect("bind");
    let addr = server.local_addr().expect("addr");
    let token = CancellationToken::new();
    let task = tokio::spawn(server.serve(token.clone()));
    (addr, token.drop_guard(), task)
}

async fn udp_exchange(server: SocketAddr, message: &[u8]) -> Vec<u8> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("client");
    socket.send_to(message, server).await.expect("send");
    let mut buf = [0u8; 1500];
    let (len, _) = timeout(LIMIT, socket.recv_from(&mut buf))
        .await
        .expect("answer within timeout")
        .expect("recv");
    buf[..len].to_vec()
}

#[tokio::test]
async fn udp_answers_zone_names_and_refuses_others() {
    let (addr, _guard, _task) = start().await;

    let response = udp_exchange(addr, &query(0x1234, "crm.clickmax-2.test", TYPE_A)).await;
    assert_eq!(&response[..2], &[0x12, 0x34]);
    assert_eq!((rcode(&response), ancount(&response)), (0, 1));
    assert_eq!(
        &response[response.len() - 4..],
        &Ipv4Addr::LOCALHOST.octets()
    );

    let response = udp_exchange(addr, &query(2, "API.Lane.TEST", TYPE_AAAA)).await;
    assert_eq!((rcode(&response), ancount(&response)), (0, 1));
    assert_eq!(
        &response[response.len() - 16..],
        &Ipv6Addr::LOCALHOST.octets()
    );

    let response = udp_exchange(addr, &query(3, "example.com", TYPE_A)).await;
    assert_eq!((rcode(&response), ancount(&response)), (5, 0), "REFUSED");

    let response = udp_exchange(addr, &[0, 9, 0x01, 0, 0, 1, 0, 0, 0, 0, 0, 0, 3]).await;
    assert_eq!((rcode(&response), response.len()), (1, 12), "FORMERR");
}

#[tokio::test]
async fn tcp_answers_several_queries_per_connection() {
    let (addr, _guard, _task) = start().await;
    let mut stream = timeout(LIMIT, TcpStream::connect(addr))
        .await
        .expect("connect in time")
        .expect("connect");
    for (id, name, expected) in [(1u16, "a.test", 0u8), (2, "a.example", 5)] {
        let message = query(id, name, TYPE_A);
        let len = u16::try_from(message.len()).expect("len");
        stream.write_all(&len.to_be_bytes()).await.expect("len");
        stream.write_all(&message).await.expect("query");
        let mut prefix = [0u8; 2];
        timeout(LIMIT, stream.read_exact(&mut prefix))
            .await
            .expect("answer in time")
            .expect("prefix");
        let mut response = vec![0u8; usize::from(u16::from_be_bytes(prefix))];
        stream.read_exact(&mut response).await.expect("response");
        assert_eq!(&response[..2], &id.to_be_bytes());
        assert_eq!(rcode(&response), expected);
    }
}

/// Interop with a real client: `dig` sends EDNS cookies and checks the response format.
#[tokio::test]
#[ignore = "needs dig"]
async fn dig_resolves_zone_names() {
    let (addr, _guard, _task) = start().await;
    for (args, expected) in [
        (vec!["a.lane.test", "A"], "127.0.0.1"),
        (vec!["a.lane.test", "AAAA"], "::1"),
        (vec!["+tcp", "a.lane.test", "A"], "127.0.0.1"),
    ] {
        let output = timeout(
            LIMIT,
            tokio::process::Command::new("dig")
                .args([
                    "@127.0.0.1",
                    "-p",
                    &addr.port().to_string(),
                    "+short",
                    "+tries=1",
                ])
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("dig in time")
        .expect("run dig");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    }
    let output = tokio::process::Command::new("dig")
        .args([
            "@127.0.0.1",
            "-p",
            &addr.port().to_string(),
            "+tries=1",
            "example.com",
        ])
        .kill_on_drop(true)
        .output()
        .await
        .expect("run dig");
    assert!(String::from_utf8_lossy(&output.stdout).contains("status: REFUSED"));
}

#[tokio::test]
async fn shutdown_stops_serving_and_frees_the_port() {
    let listen = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let server = DnsServer::bind(listen, Zone::new("test").expect("zone"))
        .await
        .expect("bind");
    let addr = server.local_addr().expect("addr");
    let token = CancellationToken::new();
    let task = tokio::spawn(server.serve(token.clone()));
    let idle = TcpStream::connect(addr).await.expect("idle client");
    token.cancel();
    timeout(LIMIT, task)
        .await
        .expect("stops in time")
        .expect("join");
    drop(idle);
    let rebound = DnsServer::bind(addr, Zone::new("test").expect("zone")).await;
    assert!(rebound.is_ok(), "port is free after shutdown: {rebound:?}");
}
