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

/// Zero retention limits would delete everything at the next cleanup, so each is
/// refused at its own key and line; the smallest nonzero values load.
#[test]
fn zero_retention_is_rejected() {
    for (source, path, line) in [
        (
            "logs:\n  retention:\n    days: 0\n",
            "logs.retention.days",
            3,
        ),
        (
            "logs:\n  retention:\n    max_size: 0B\n",
            "logs.retention.max_size",
            3,
        ),
        ("history:\n  retention: 0s\n", "history.retention", 2),
        (
            "history:\n  remote_hosts_retention: 0d\n",
            "history.remote_hosts_retention",
            2,
        ),
        ("metrics:\n  retention: 0m\n", "metrics.retention", 2),
    ] {
        let error = parse_user(source).expect_err(source);
        assert!(
            error.message.starts_with(&format!("{path}: must be")),
            "{source}: {}",
            error.message
        );
        assert!(error.message.contains("delete"), "{}", error.message);
        assert_eq!(error.location.line, line, "{source}: {}", error.message);
    }
    let smallest = parse_user(
        "logs: { retention: { days: 1, max_size: 1B } }\nhistory: { retention: 1s, remote_hosts_retention: 1s }\nmetrics: { retention: 1s }\n",
    )
    .expect("nonzero limits");
    assert_eq!(smallest.logs.retention.days, 1);
    assert_eq!(smallest.logs.retention.max_size.0, 1);
    // A lenient load drops only the zero limit; the other keys keep their values.
    let (config, issues) = parse_user_lenient(
        "config.yaml",
        "logs:\n  retention:\n    days: 0\n    max_size: 1GiB\n",
    );
    assert_eq!(config.logs.retention.days, LogRetention::default().days);
    assert_eq!(config.logs.retention.max_size.0, 1 << 30);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(issues[0].message.starts_with("logs.retention.days:"));
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
