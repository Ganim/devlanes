use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, str::FromStr};

/// Optional display overrides for a tag; services may also use undeclared tags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TagConfig {
    /// Terminal palette color; inferred by the resolver when omitted.
    pub color: Option<TagColor>,
    /// Human-readable description available to the TUI.
    pub description: Option<String>,
}

macro_rules! palette {
    ($($variant:ident => $name:literal),* $(,)?) => {
        /// Fixed terminal palette for tag color overrides.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
        pub enum TagColor { $(#[serde(rename = $name)] $variant,)* }

        impl TagColor {
            /// Return the canonical configuration spelling.
            pub fn as_str(self) -> &'static str { match self { $(Self::$variant => $name,)* } }
        }

        impl FromStr for TagColor {
            type Err = String;
            /// Parse a palette name and suggest a nearby spelling for invalid values.
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($name => Ok(Self::$variant),)*
                    _ => {
                        let mut message = format!("unknown tag color `{value}`");
                        let names = [$($name,)*];
                        if let Some(suggestion) = names.iter().max_by(|a,b| strsim::jaro_winkler(value,a).total_cmp(&strsim::jaro_winkler(value,b)))
                            && strsim::jaro_winkler(value,suggestion) > 0.75 {
                            message.push_str(&format!("; did you mean `{suggestion}`?"));
                        } else { message.push_str(&format!("; expected one of {}",names.join(", "))); }
                        Err(message)
                    }
                }
            }
        }
    };
}

palette! {
    Red => "red", Green => "green", Yellow => "yellow", Blue => "blue",
    Magenta => "magenta", Cyan => "cyan", White => "white", Gray => "gray",
    BrightRed => "bright-red", BrightGreen => "bright-green", BrightYellow => "bright-yellow",
    BrightBlue => "bright-blue", BrightMagenta => "bright-magenta", BrightCyan => "bright-cyan",
}

impl<'de> Deserialize<'de> for TagColor {
    /// Validate inside the YAML deserializer to retain source coordinates.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for TagColor {
    /// Display the canonical palette name.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
