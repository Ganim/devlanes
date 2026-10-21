use std::time::Duration;

mod migrate;

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
