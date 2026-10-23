use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, str::FromStr, time::Duration};

/// A text, number or boolean value. Process environments receive it as text, such as
/// `true` or `20000`.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["acme",20000,true]))]
pub enum Scalar {
    /// Text, used as written.
    #[schemars(extend("examples" = ["acme"]))]
    String(String),
    /// Boolean, passed to processes as `true` or `false`.
    #[schemars(extend("examples" = [true,false]))]
    Bool(bool),
    /// Whole number, such as a port or a database index.
    #[schemars(extend("examples" = [20000]))]
    Integer(i64),
    /// Decimal number, passed to processes as text.
    #[schemars(extend("examples" = [1.5]))]
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

/// A plain value or text with `${{ ... }}` expressions, evaluated for each lane with names
/// such as `lane`, `self`, `services`, `infra` and `env`. A value that is one whole
/// expression keeps its type, which must fit the setting. Write `${{ '${{' }}` for a
/// literal `${{`.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(rename = "Templated_{T}")]
#[schemars(extend("examples" = ["${{ lane.index }}"]))]
pub enum Templated<T> {
    /// Plain value, used as written.
    Literal(T),
    /// Text containing `${{ ... }}`, evaluated for each lane.
    #[schemars(extend("pattern" = "\\$\\{\\{", "examples" = ["${{ lane.index }}"]))]
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
            .map_err(|error| {
                serde::de::Error::custom(format!(
                    "{}; a `${{{{ ... }}}}` expression is also allowed",
                    plain_type_names(&error.to_string())
                ))
            })
    }
}

/// Replace Rust primitive names in serde messages with the words used in YAML docs.
pub(crate) fn plain_type_names(message: &str) -> String {
    let mut message = message.to_owned();
    for (rust, plain) in [
        ("expected u16", "expected a whole number from 0 to 65535"),
        (
            "expected u32",
            "expected a whole number from 0 to 4294967295",
        ),
        ("expected u64", "expected a nonnegative whole number"),
        ("expected i64", "expected a whole number"),
        ("expected f64", "expected a number"),
        ("expected a boolean", "expected true or false"),
    ] {
        message = message.replace(rust, plain);
    }
    message
}

impl<T: Clone + serde::de::DeserializeOwned> Templated<T> {
    /// Resolve a template without coercing an expression result into another type.
    pub fn evaluate(&self, context: &crate::ExpressionContext) -> Result<T, crate::ConfigError> {
        match self {
            Self::Literal(value) => Ok(value.clone()),
            Self::Expression(text) => serde_json::from_value(crate::evaluate(text, context)?)
                .map_err(|error| {
                    crate::ConfigError::plain(format!(
                        "expression result has the wrong type: {}",
                        plain_type_names(&error.to_string())
                    ))
                }),
        }
    }
}

/// A duration: a whole number followed by `ms`, `s`, `m`, `h` or `d`, such as `500ms`, `15s`
/// or `10m`. Units cannot be combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(into = "String")]
#[schemars(with = "String")]
#[schemars(extend("pattern" = "^[0-9]+(ms|s|m|h|d)$", "examples" = ["15s","10m"]))]
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
#[schemars(extend("pattern" = "^\\s*([0-9]+\\.?[0-9]*|\\.[0-9]+)\\s*%\\s*$", "examples" = ["75%","12.5%"]))]
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
#[schemars(extend("pattern" = "^[0-9]+(B|KB|MB|GB|KiB|MiB|GiB)$", "examples" = ["5GiB"]))]
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
