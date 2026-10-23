//! The real NRPT round trip. It changes `HKEY_LOCAL_MACHINE` and shows two
//! UAC prompts, so it is ignored by default; run it from an interactive
//! session with nothing (no devlanes daemon) on `127.53.0.1:53`:
//!
//! ```sh
//! cargo test -p devlanes-resolver-nrpt --test it -- --ignored
//! ```
#![cfg(windows)]

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use devlanes_resolver_dns::DnsServer;
use devlanes_resolver_nrpt::{Nrpt, RuleState, zone};
use tokio_util::sync::CancellationToken;

/// Every lookup of `host`, through the system resolver.
async fn resolve(host: &str) -> Vec<SocketAddr> {
    let host = format!("{host}:80");
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            host.to_socket_addrs()
                .map(Iterator::collect)
                .unwrap_or_default()
        }),
    )
    .await
    .expect("lookup within 10 s")
    .expect("join")
}

#[tokio::test]
#[ignore = "needs admin"]
async fn a_rule_sends_the_tld_to_the_devlanes_server() {
    let zone = zone("nrpt-check.test").expect("zone");
    let server = DnsServer::bind(devlanes_resolver_dns::default_listen(), zone.clone())
        .await
        .expect("bind 127.53.0.1:53");
    let token = CancellationToken::new();
    let serving = tokio::spawn(server.serve(token.clone()));
    let stop = token.drop_guard();
    let nrpt = Nrpt::default();
    let home = tempfile::tempdir().expect("tempdir");

    let change = nrpt
        .plan_install(&zone)
        .expect("plan")
        .expect("no rule yet");
    let applied = {
        let (nrpt, change, dir) = (nrpt.clone(), change.clone(), home.path().to_owned());
        tokio::task::spawn_blocking(move || nrpt.apply(&change, &dir))
            .await
            .expect("join")
    };
    applied.expect("install");
    let addresses = resolve("x.nrpt-check.test").await;

    let removal = nrpt.plan_removal().expect("plan").expect("a rule");
    let removed = {
        let (nrpt, dir) = (nrpt.clone(), home.path().to_owned());
        tokio::task::spawn_blocking(move || nrpt.apply(&removal, &dir))
            .await
            .expect("join")
    };
    removed.expect("remove");
    assert_eq!(
        nrpt.status(&zone).expect("status").state,
        RuleState::Missing
    );
    assert!(
        !addresses.is_empty() && addresses.iter().all(|addr| addr.ip().is_loopback()),
        "{addresses:?}"
    );
    assert!(
        std::fs::read_dir(home.path())
            .expect("home")
            .next()
            .is_none(),
        "the .reg file is deleted"
    );
    drop(stop);
    let _ = tokio::time::timeout(Duration::from_secs(5), serving).await;
}
