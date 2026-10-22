use devlanes_config::*;
use serde_json::json;
use std::time::Duration;

/// Omitted sections and partially specified sections retain their documented defaults.
#[test]
fn retention_defaults_and_overrides() {
    let defaults = parse_user("{}").expect("user defaults");
    assert_eq!(defaults.history.retention.0, DEFAULT_RUN_HISTORY_RETENTION);
    // A week of finished runs and monitor history unless configured.
    let week = Duration::from_secs(7 * 86_400);
    assert_eq!(defaults.history.retention.0, week);
    assert_eq!(defaults.metrics.retention.0, week);
    assert_eq!(DEFAULT_METRICS_RETENTION, week);
    assert_eq!(
        defaults.history.remote_hosts_retention.0,
        DEFAULT_REMOTE_HOST_RETENTION
    );
    assert_eq!(defaults.state.max_size.0, DEFAULT_MAX_SIZE_BYTES);
    let config = parse_user(
        "history: { retention: 14d, remote_hosts_retention: 180d }\nstate: { max_size: 1GiB }\n",
    )
    .expect("overrides");
    assert_eq!(config.history.retention.0, Duration::from_secs(14 * 86_400));
    assert_eq!(
        config.history.remote_hosts_retention.0,
        Duration::from_secs(180 * 86_400)
    );
    assert_eq!(config.state.max_size.0, 1 << 30);
    let partial = parse_user("history: { retention: 7d }\nstate: {}\n").expect("partial sections");
    assert_eq!(
        partial.history.remote_hosts_retention.0,
        DEFAULT_REMOTE_HOST_RETENTION
    );
    assert_eq!(partial.state.max_size.0, DEFAULT_MAX_SIZE_BYTES);
}

/// Retention fields remain typed and reject unknown keys and unusable disk budgets.
#[test]
fn retention_validation_and_schema() {
    for source in [
        "history: { retention: tomorrow }",
        "history: { remote_hosts_retention: -1d }",
        "history: { unknown: 30d }",
        "state: { max_size: 512 }",
        "state: { max_size: 0B }",
        "state: { unknown: 1GiB }",
    ] {
        assert!(parse_user(source).is_err(), "accepted {source}");
    }
    let validator = jsonschema::validator_for(&user_schema()).expect("user schema");
    assert!(validator.is_valid(&json!({"history":{"retention":"30d","remote_hosts_retention":"90d"},"state":{"max_size":"512MiB"}})));
    assert!(!validator.is_valid(&json!({"history":{"retention":30}})));
    assert!(!validator.is_valid(&json!({"state":{"unknown":"512MiB"}})));
}
