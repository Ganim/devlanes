use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, str::FromStr, time::Duration};

/// A scalar suitable for environment variables and free-form lane variables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Scalar {
    String(String),
    Bool(bool),
    Integer(i64),
    Float(f64),
}

impl fmt::Display for Scalar {
    /// Render a scalar for an operating-system environment variable.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(value) => value.fmt(f),
            Self::Bool(value) => value.fmt(f),
            Self::Integer(value) => value.fmt(f),
            Self::Float(value) => value.fmt(f),
        }
    }
}

/// A literal value or a `${{ ... }}` template, checked against `T` after evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(rename = "Templated_{T}")]
pub enum Templated<T> {
    Literal(T),
    Expression(String),
}

impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for Templated<T> {
    /// Distinguish expression strings before attempting the literal type.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(text) = value.as_str().filter(|s| s.contains("${{")) {
            return Ok(Self::Expression(text.into()));
        }
        serde_json::from_value(value)
            .map(Self::Literal)
            .map_err(serde::de::Error::custom)
    }
}

impl<T: Clone + serde::de::DeserializeOwned> Templated<T> {
    /// Resolve a template without coercing an expression result into another type.
    pub fn evaluate(&self, context: &crate::ExpressionContext) -> Result<T, crate::ConfigError> {
        match self {
            Self::Literal(value) => Ok(value.clone()),
            Self::Expression(text) => serde_json::from_value(crate::evaluate(text, context)?)
                .map_err(|_| {
                    crate::ConfigError::plain(format!(
                        "expression has the wrong type; expected {}",
                        std::any::type_name::<T>()
                    ))
                }),
        }
    }
}

/// A duration written with `ms`, `s`, `m`, `h`, or `d` units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(into = "String")]
#[schemars(with = "String")]
pub struct ConfigDuration(pub Duration);

impl FromStr for ConfigDuration {
    type Err = String;
    /// Parse an unsigned duration and reject overflow and unknown units.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let units = [
            ("ms", 1_u64),
            ("s", 1000),
            ("m", 60_000),
            ("h", 3_600_000),
            ("d", 86_400_000),
        ];
        for (unit, multiplier) in units {
            if let Some(number) = input.strip_suffix(unit) {
                return number
                    .parse::<u64>()
                    .ok()
                    .and_then(|n| n.checked_mul(multiplier))
                    .map(|n| Self(Duration::from_millis(n)))
                    .ok_or_else(|| format!("invalid duration `{input}`"));
            }
        }
        Err(format!(
            "invalid duration `{input}`; expected ms, s, m, h, or d"
        ))
    }
}

impl From<ConfigDuration> for String {
    /// Serialize losslessly, including millisecond durations.
    fn from(value: ConfigDuration) -> Self {
        format!("{}ms", value.0.as_millis())
    }
}

impl<'de> Deserialize<'de> for ConfigDuration {
    /// Parse a human-readable duration string.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A percentage with a `%` suffix, such as `75%` or `12.5%`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
#[serde(into = "String")]
#[schemars(with = "String")]
pub struct Percent(pub f64);

impl FromStr for Percent {
    type Err = String;
    /// Parse a finite, non-negative number followed by `%`.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        input
            .trim()
            .strip_suffix('%')
            .and_then(|number| number.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(Self)
            .ok_or_else(|| {
                format!("invalid percentage `{input}`; expected a number with %, such as 75%")
            })
    }
}

impl From<Percent> for String {
    /// Serialize with the `%` suffix the parser expects.
    fn from(value: Percent) -> Self {
        format!("{}%", value.0)
    }
}

impl<'de> Deserialize<'de> for Percent {
    /// Parse a percentage string; bare numbers are rejected because `75` could
    /// mean a ratio or a percentage.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A byte count with decimal or binary size units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(into = "String")]
#[schemars(with = "String")]
pub struct ByteSize(pub u64);

impl FromStr for ByteSize {
    type Err = String;
    /// Parse sizes such as `5GiB` or `100MB` with checked arithmetic.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        for (suffix, multiplier) in [
            ("GiB", 1_u64 << 30),
            ("MiB", 1 << 20),
            ("KiB", 1 << 10),
            ("GB", 1_000_000_000),
            ("MB", 1_000_000),
            ("KB", 1000),
            ("B", 1),
        ] {
            if let Some(number) = input.strip_suffix(suffix) {
                return number
                    .parse::<u64>()
                    .ok()
                    .and_then(|n| n.checked_mul(multiplier))
                    .map(Self)
                    .ok_or_else(|| format!("invalid byte size `{input}`"));
            }
        }
        Err(format!(
            "invalid byte size `{input}`; expected B, KB, MB, GB, KiB, MiB, or GiB"
        ))
    }
}

impl From<ByteSize> for String {
    /// Preserve the exact byte count in serialized output.
    fn from(value: ByteSize) -> Self {
        format!("{}B", value.0)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    /// Parse a human-readable byte size string.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
