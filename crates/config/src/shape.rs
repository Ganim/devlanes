//! Shape-directed deserialization for config values with alternative forms.
//!
//! An untagged enum buffers the value and reports only "data did not match any variant",
//! hiding unknown-key errors and their suggestions. These visitors pick the form from the
//! YAML shape instead, so a mapping goes straight through its struct's strict parser and keeps
//! its source position.

use crate::{
    ComposeFiles, HostConfig, HostRoute, InfraCommand, InfraCommandOptions, ProfileConfig,
    ProfileOptions, ReadyCheck, ReadyConfig, Run, RunScript, Scalar, ScriptSource, Started,
    TcpReady, Templated, Terminal, TerminalConfig, TerminalSettings,
};
use serde::{
    Deserialize, Deserializer,
    de::{
        self, IntoDeserializer, MapAccess, SeqAccess, Visitor,
        value::{MapAccessDeserializer, SeqAccessDeserializer},
    },
};
use std::fmt;

/// Implement `Deserialize` with a visitor whose `expecting` text names the accepted forms.
macro_rules! shape {
    ($ty:ty, $expected:literal, $($methods:tt)*) => {
        impl<'de> Deserialize<'de> for $ty {
            /// Select the form by YAML shape without buffering the value.
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct Shape;
                impl<'de> Visitor<'de> for Shape {
                    type Value = $ty;
                    /// Describe the accepted YAML forms in plain words.
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str($expected)
                    }
                    $($methods)*
                }
                d.deserialize_any(Shape)
            }
        }
    };
}

/// Accept a string as the given single-field variant.
macro_rules! text {
    ($variant:path) => {
        /// Parse the string form.
        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            Ok($variant(value.into()))
        }
    };
}

/// Accept a mapping through the given struct's own strict parser.
macro_rules! mapping {
    ($variant:path, $ty:ty) => {
        /// Let the struct report unknown keys itself.
        fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            <$ty>::deserialize(MapAccessDeserializer::new(map)).map($variant)
        }
    };
}

/// Accept a list through the given element parser.
macro_rules! sequence {
    ($variant:path, $ty:ty) => {
        /// Parse list elements without losing their source positions.
        fn visit_seq<S: SeqAccess<'de>>(self, seq: S) -> Result<Self::Value, S::Error> {
            <$ty>::deserialize(SeqAccessDeserializer::new(seq)).map($variant)
        }
    };
}

/// Keep `${{ ... }}` text for evaluation; any other text is literal.
fn templated(text: &str) -> Templated<String> {
    if text.contains("${{") {
        Templated::Expression(text.into())
    } else {
        Templated::Literal(text.into())
    }
}

shape!(Run, "a shell command string or an argument list",
    /// Parse shell text.
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Run, E> {
        Ok(Run::Shell(templated(value)))
    }
    sequence!(Run::Argv, Vec<Templated<String>>);
);

shape!(RunScript, "`file#name` or a mapping with source and name",
    text!(RunScript::Short);
    mapping!(RunScript::Object, ScriptSource);
);

shape!(HostConfig, "a hostname or a host route mapping",
    /// Parse a hostname.
    fn visit_str<E: de::Error>(self, value: &str) -> Result<HostConfig, E> {
        Ok(HostConfig::Name(templated(value)))
    }
    mapping!(HostConfig::Route, HostRoute);
);

shape!(ProfileConfig, "a selector list or a profile mapping",
    sequence!(ProfileConfig::Selectors, Vec<String>);
    mapping!(ProfileConfig::Object, ProfileOptions);
);

shape!(ReadyConfig, "`started` or a readiness-check mapping",
    /// Only `started` is a valid text readiness policy.
    fn visit_str<E: de::Error>(self, value: &str) -> Result<ReadyConfig, E> {
        if value == "started" {
            Ok(ReadyConfig::Started(Started::Started))
        } else {
            Err(E::invalid_value(de::Unexpected::Str(value), &self))
        }
    }
    mapping!(ReadyConfig::Check, ReadyCheck);
);

shape!(TcpReady, "true or a named port",
    text!(TcpReady::Port);
    /// Parse the primary-port switch.
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<TcpReady, E> {
        Ok(TcpReady::Enabled(value))
    }
);

shape!(TerminalConfig, "a terminal app name or a mapping with app and progress",
    /// Parse the app name; an unknown one lists the known apps.
    fn visit_str<E: de::Error>(self, value: &str) -> Result<TerminalConfig, E> {
        Terminal::deserialize(value.into_deserializer()).map(TerminalConfig::App)
    }
    mapping!(TerminalConfig::Settings, TerminalSettings);
);

shape!(ComposeFiles, "a Compose file name or a list of them",
    text!(ComposeFiles::File);
    sequence!(ComposeFiles::Files, Vec<String>);
);

shape!(InfraCommand, "a shell command string, an argument list, or a command mapping",
    /// Parse the shell shorthand.
    fn visit_str<E: de::Error>(self, value: &str) -> Result<InfraCommand, E> {
        Ok(InfraCommand::Command(Run::Shell(templated(value))))
    }
    /// Parse the argument-list shorthand.
    fn visit_seq<S: SeqAccess<'de>>(self, seq: S) -> Result<InfraCommand, S::Error> {
        Vec::deserialize(SeqAccessDeserializer::new(seq))
            .map(|argv| InfraCommand::Command(Run::Argv(argv)))
    }
    mapping!(InfraCommand::Object, Box<InfraCommandOptions>);
);

shape!(Scalar, "a string, boolean, or number",
    text!(Scalar::String);
    /// Keep booleans typed.
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Scalar, E> {
        Ok(Scalar::Bool(value))
    }
    /// Keep signed integers typed.
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Scalar, E> {
        Ok(Scalar::Integer(value))
    }
    /// Reject integers above the signed range instead of wrapping them.
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Scalar, E> {
        i64::try_from(value)
            .map(Scalar::Integer)
            .map_err(|_| E::invalid_value(de::Unexpected::Unsigned(value), &self))
    }
    /// Keep decimals typed.
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Scalar, E> {
        Ok(Scalar::Float(value))
    }
);
