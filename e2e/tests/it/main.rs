//! End-to-end tests: every test drives the real `devlanes` binary in its own [`Lab`].

mod cli;
// Docker test names must start with `docker_` including the module path, for the CI filter.
mod docker_full;
mod lanes;
mod lifecycle;
mod proxy;
mod with;

use std::time::Duration;

use devlanes_e2e::{Error, Result};
use serde_json::Value;

/// Upper bound for one test, including startup and teardown commands.
const BUDGET: Duration = Duration::from_secs(150);

/// Turns a missing value into a failure that says what was looked for.
fn need<T>(value: Option<T>, what: &str) -> Result<T> {
    value.ok_or_else(|| Error::Unexpected(format!("missing {what}")))
}

/// Unit names of a lane summary in a given state.
fn units_in(lane: &Value, state: &str) -> Vec<String> {
    lane["units"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|unit| unit["state"] == state)
        .filter_map(|unit| unit["name"].as_str().map(str::to_owned))
        .collect()
}
