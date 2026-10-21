//! Real install against this machine's `/etc/hosts` and sudo.
//!
//! Run explicitly with a release helper, from a terminal where sudo can prompt:
//!
//! ```sh
//! cargo build --release -p devlanes-hosts-helper
//! DEVLANES_HOSTS_HELPER=$PWD/target/release/devlanes-hosts \
//!   cargo test -p devlanes-resolver-hosts --test it -- --ignored --nocapture
//! ```

// The helper and its sudo rule only exist on Unix.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::net::ToSocketAddrs as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use devlanes_resolver_hosts::install::{self, HELPER_SOURCE_ENV};
use devlanes_resolver_hosts::{HostsClient, SystemExecutor, SystemFs};

const LIMIT: Duration = Duration::from_secs(120);

#[tokio::test]
#[ignore = "needs sudo: installs into /usr/local/libexec and /etc/sudoers.d"]
async fn installs_applies_and_uninstalls() {
    let source =
        PathBuf::from(std::env::var_os(HELPER_SOURCE_ENV).expect("set DEVLANES_HOSTS_HELPER"));
    let run = async {
        install::install(&SystemExecutor, &SystemFs, &source)
            .await
            .expect("install");
        let client =
            HostsClient::new(Arc::new(SystemExecutor), Arc::new(SystemFs), None).expect("client");
        let name = format!("devlanes-it-{}.localhost", std::process::id());
        let names: BTreeSet<String> = [name.clone()].into();
        let applied = client
            .apply(&names)
            .await
            .expect("apply without a password");
        assert_eq!(applied.names, 1);
        assert!(client.status().await.expect("status").contains(&name));
        let lookup = name.clone();
        let addrs = tokio::task::spawn_blocking(move || {
            (lookup.as_str(), 0)
                .to_socket_addrs()
                .map(Iterator::collect::<Vec<_>>)
        })
        .await
        .expect("join")
        .expect("resolves");
        assert!(
            addrs.iter().all(|addr| addr.ip().is_loopback()),
            "{addrs:?}"
        );

        let warnings = install::uninstall(&SystemExecutor, &SystemFs)
            .await
            .expect("uninstall");
        assert!(warnings.is_empty(), "{warnings:?}");
        let hosts = tokio::fs::read_to_string("/etc/hosts")
            .await
            .expect("read hosts");
        assert!(!hosts.contains("# devlanes begin"));
        assert!(!std::path::Path::new(devlanes_resolver_hosts::INSTALL_PATH).exists());
    };
    tokio::time::timeout(LIMIT, run)
        .await
        .expect("finishes in time");
}
