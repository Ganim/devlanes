use std::time::Duration;

/// The config crate repeats the DNS zone's TLD rule to reject a bad `hostnames.tld`
/// at load; both must accept and refuse the same names.
#[test]
fn config_and_resolver_agree_on_tlds() {
    let long = format!("{}.test", "a".repeat(64));
    let longest = format!("{}.test", vec!["a".repeat(63); 4].join("."));
    for tld in [
        "test",
        "Test.",
        "internal",
        "dev.internal",
        "home.arpa",
        "lab.home.arpa",
        "a.b.test",
        "dev.localhost",
        "x-1.test",
        "",
        ".",
        "..",
        "a..test",
        "-x.test",
        "x-.test",
        "x_y.test",
        "café.test",
        "123",
        "1.123",
        "localhost",
        "local",
        "dev",
        "com",
        "example",
        "contest",
        "test.com",
        "xhome.arpa",
        "test..",
        &long,
        &longest,
    ] {
        let source = format!("hostnames: {{ tld: {} }}", serde_json::json!(tld));
        let config = devlanes_config::parse_user(&source);
        let zone = devlanes_resolver_dns::Zone::new(tld);
        assert_eq!(
            config.is_ok(),
            zone.is_ok(),
            "{tld:?}: config {:?}, zone {zone:?}",
            config.err().map(|error| error.message)
        );
    }
    assert_eq!(
        devlanes_resolver_dns::RESERVED_ZONES,
        ["test", "internal", "home.arpa"]
    );
}

/// Config and state remain independent crates but must agree on retention defaults.
#[test]
fn config_and_state_retention_defaults_match() {
    let config = devlanes_config::UserConfig::default();
    let policy = devlanes_state::RetentionPolicy::default();
    assert_eq!(
        devlanes_config::DEFAULT_RUN_HISTORY_RETENTION,
        devlanes_state::DEFAULT_RUN_HISTORY_RETENTION
    );
    assert_eq!(
        devlanes_config::DEFAULT_METRICS_RETENTION,
        devlanes_state::DEFAULT_METRICS_RETENTION
    );
    assert_eq!(
        devlanes_config::DEFAULT_REMOTE_HOST_RETENTION,
        devlanes_state::DEFAULT_REMOTE_HOST_RETENTION
    );
    assert_eq!(
        devlanes_config::DEFAULT_MAX_SIZE_BYTES,
        devlanes_state::DEFAULT_MAX_SIZE_BYTES
    );
    assert_eq!(config.history.retention.0, policy.run_history);
    assert_eq!(config.history.remote_hosts_retention.0, policy.remote_hosts);
    assert_eq!(config.state.max_size.0, policy.max_size_bytes);
    assert_eq!(config.metrics.retention.0, policy.metrics);
    assert_eq!(
        Duration::from_secs(u64::from(config.logs.retention.days) * 86_400),
        policy.logs
    );
}

/// The Windows binary carries its manifest, whose `detached` console policy
/// keeps the logon start windowless. Windows stores an embedded manifest as
/// plain text in the binary's resources.
#[cfg(windows)]
#[test]
fn windows_binary_embeds_the_console_policy() {
    let binary = std::fs::read(env!("CARGO_BIN_EXE_devlanes")).expect("devlanes.exe");
    let policy = br#"<consoleAllocationPolicy xmlns="http://schemas.microsoft.com/SMI/2024/WindowsSettings">detached</consoleAllocationPolicy>"#;
    assert!(
        binary.windows(policy.len()).any(|window| window == policy),
        "no consoleAllocationPolicy in the embedded manifest"
    );
}
