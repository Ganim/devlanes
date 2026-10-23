use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, str::FromStr};

/// Settings for one tag used in `services.<name>.tags`. Declaring a tag here is optional:
/// services can use any tag and `+tag` selectors work without it. Declare one to pin its
/// color or describe it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"color":"cyan","description":"HTTP APIs"}]))]
pub struct TagConfig {
    /// Color of services with this tag in the TUI and CLI output. When a service has several
    /// tags with a color, the first one in its `tags` list wins. Without one, devlanes infers
    /// a color from category words such as `api` (blue), `web` (green) or `worker` (magenta)
    /// in the service's tags, preset or name.
    pub color: Option<TagColor>,
    /// Note on what the tag groups, for people reading the config.
    #[schemars(extend("examples" = ["HTTP APIs","Background workers"]))]
    pub description: Option<String>,
}

macro_rules! palette {
    ($($variant:ident => $name:literal),* $(,)?) => {
        /// A color from the terminal's basic palette; the exact shade follows the terminal
        /// theme, and `bright-` variants are the lighter shades.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
        #[schemars(extend("examples" = [$($name,)*]))]
        pub enum TagColor {
            $(#[doc = concat!("Show the tag's services in the terminal's ", $name, " color.")]
            #[schemars(extend("examples" = [$name]))]
            #[serde(rename = $name)] $variant,)*
        }

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
