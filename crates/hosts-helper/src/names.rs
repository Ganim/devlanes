//! Hostname validation for names written to `/etc/hosts`.
//!
//! Names arrive from an unprivileged caller, so everything is checked here and
//! nothing is normalised: a name that is not already lowercase, without a trailing
//! dot, is refused instead of rewritten.

use std::collections::BTreeSet;
use std::fmt;

/// Most names one `apply` accepts. Each name becomes two lines, and macOS and glibc
/// scan the file linearly on lookups, so a runaway caller must not grow it unbounded.
pub const MAX_NAMES: usize = 4096;

/// Longest hostname in presentation form without the trailing dot (RFC 1035).
pub const MAX_NAME_LEN: usize = 253;

/// Longest DNS label (RFC 1035).
pub const MAX_LABEL_LEN: usize = 63;

/// Largest `apply` input: every name at full length plus a CRLF.
pub const MAX_INPUT_BYTES: usize = MAX_NAMES * (MAX_NAME_LEN + 2);

/// Special-use suffixes a `--tld` may name or sit under.
///
/// `localhost` and `test` are reserved by RFC 6761, `home.arpa` by RFC 8375 and
/// `internal` by ICANN for private use. None of them can belong to a public site,
/// so pointing their names at loopback cannot hijack anyone else's domain.
pub const RESERVED_SUFFIXES: &[&str] = &["localhost", "test", "internal", "home.arpa"];

/// Suffix every caller may use without `--tld`.
pub const DEFAULT_SUFFIX: &str = "localhost";

/// A rejected name or TLD, with the reason shown to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameError {
    /// The rejected value, as given.
    pub value: String,
    /// Why it was rejected.
    pub reason: &'static str,
}

impl fmt::Display for NameError {
    /// `invalid name "x": reason`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid name {:?}: {}", self.value, self.reason)
    }
}

impl std::error::Error for NameError {}

/// Failure to read the `apply` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    /// The input is larger than [`MAX_INPUT_BYTES`].
    TooLarge,
    /// The input is not UTF-8.
    NotUtf8,
    /// More than [`MAX_NAMES`] distinct names.
    TooMany {
        /// Distinct names counted before stopping.
        count: usize,
    },
    /// A line is not an acceptable name.
    Name {
        /// 1-based line number.
        line: usize,
        /// The validation failure.
        error: NameError,
    },
}

impl fmt::Display for InputError {
    /// Human-readable message for stderr.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "input is larger than {MAX_INPUT_BYTES} bytes"),
            Self::NotUtf8 => f.write_str("input is not UTF-8"),
            Self::TooMany { count } => {
                write!(f, "more than {MAX_NAMES} names (stopped at {count})")
            }
            Self::Name { line, error } => write!(f, "line {line}: {error}"),
        }
    }
}

impl std::error::Error for InputError {}

/// The suffixes names must sit under: always `localhost`, plus an optional TLD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suffixes {
    suffixes: Vec<String>,
}

impl Suffixes {
    /// `localhost` plus `tld` when given; `tld` must pass [`validate_tld`].
    pub fn new(tld: Option<&str>) -> Result<Self, NameError> {
        let mut suffixes = vec![DEFAULT_SUFFIX.to_owned()];
        if let Some(tld) = tld {
            let tld = validate_tld(tld)?;
            if tld != DEFAULT_SUFFIX {
                suffixes.push(tld);
            }
        }
        Ok(Self { suffixes })
    }

    /// Accepted suffixes, `localhost` first.
    pub fn as_slice(&self) -> &[String] {
        &self.suffixes
    }

    /// Whether `name` is strictly below one of the suffixes (the suffix alone is not).
    pub fn allows(&self, name: &str) -> bool {
        self.suffixes
            .iter()
            .any(|suffix| is_strictly_under(name, suffix))
    }
}

/// Whether `name` ends with `.suffix` on a label boundary.
fn is_strictly_under(name: &str, suffix: &str) -> bool {
    name.len() > suffix.len() + 1
        && name.ends_with(suffix)
        && name.as_bytes()[name.len() - suffix.len() - 1] == b'.'
}

/// Checks hostname syntax: lowercase LDH labels of 1 to 63 bytes, at most 253 in total.
///
/// Wildcards, underscores, uppercase, a trailing dot and IP literals all fail here.
pub fn check_syntax(value: &str) -> Result<(), NameError> {
    let invalid = |reason| NameError {
        value: value.to_owned(),
        reason,
    };
    if value.is_empty() {
        return Err(invalid("it is empty"));
    }
    if value.len() > MAX_NAME_LEN {
        return Err(invalid("it is longer than 253 characters"));
    }
    if value.contains('*') {
        return Err(invalid("wildcards cannot be written to /etc/hosts"));
    }
    for label in value.split('.') {
        if label.is_empty() {
            return Err(invalid("it has an empty label"));
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(invalid("a label is longer than 63 characters"));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(invalid(
                "labels may only hold lowercase letters, digits and hyphens",
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(invalid("a label starts or ends with a hyphen"));
        }
    }
    Ok(())
}

/// Validates a caller-supplied TLD and returns it.
///
/// The value comes from the unprivileged caller, so being the user's configured TLD
/// proves nothing. It must equal or sit under one of [`RESERVED_SUFFIXES`]; otherwise
/// any process running as the user could point `example.com` at loopback for every
/// account on the machine.
pub fn validate_tld(tld: &str) -> Result<String, NameError> {
    check_syntax(tld)?;
    let reserved = RESERVED_SUFFIXES
        .iter()
        .any(|suffix| tld == *suffix || is_strictly_under(tld, suffix));
    if reserved {
        Ok(tld.to_owned())
    } else {
        Err(NameError {
            value: tld.to_owned(),
            reason: "the TLD must be localhost, test, internal or home.arpa, or a domain under one of them",
        })
    }
}

/// Validates one hostname against syntax and `suffixes`.
pub fn validate_name(name: &str, suffixes: &Suffixes) -> Result<(), NameError> {
    check_syntax(name)?;
    if suffixes.allows(name) {
        Ok(())
    } else {
        Err(NameError {
            value: name.to_owned(),
            reason: "it is not under an allowed suffix",
        })
    }
}

/// Parses `apply` input: one name per line, `\n` or `\r\n`, blank lines ignored.
///
/// Duplicates collapse. One bad line fails the whole input so a caller bug never
/// leaves a partial block behind.
pub fn parse_names(input: &[u8], suffixes: &Suffixes) -> Result<BTreeSet<String>, InputError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(InputError::TooLarge);
    }
    let text = std::str::from_utf8(input).map_err(|_| InputError::NotUtf8)?;
    let mut names = BTreeSet::new();
    for (index, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        validate_name(line, suffixes).map_err(|error| InputError::Name {
            line: index + 1,
            error,
        })?;
        names.insert(line.to_owned());
        if names.len() > MAX_NAMES {
            return Err(InputError::TooMany { count: names.len() });
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn localhost_only() -> Suffixes {
        Suffixes::new(None).expect("default suffixes")
    }

    #[test]
    fn name_validation_table() {
        let test = Suffixes::new(Some("test")).expect("test tld");
        let accepted = [
            "a.localhost",
            "crm.clickmax-2.localhost",
            "x1.y-2.z3.localhost",
            "0.localhost",
            "api.acme.test",
        ];
        for name in accepted {
            assert_eq!(validate_name(name, &test), Ok(()), "{name}");
        }
        let long_label = format!("{}.localhost", "a".repeat(64));
        let max_label = format!("{}.localhost", "a".repeat(63));
        assert_eq!(validate_name(&max_label, &test), Ok(()));
        let rejected: &[(&str, &str)] = &[
            ("", "it is empty"),
            ("localhost", "it is not under an allowed suffix"),
            ("test", "it is not under an allowed suffix"),
            ("example.com", "it is not under an allowed suffix"),
            ("evil-localhost", "it is not under an allowed suffix"),
            ("a.localhost.com", "it is not under an allowed suffix"),
            ("a.internal", "it is not under an allowed suffix"),
            (
                "*.acme.localhost",
                "wildcards cannot be written to /etc/hosts",
            ),
            ("a*.localhost", "wildcards cannot be written to /etc/hosts"),
            (
                "A.localhost",
                "labels may only hold lowercase letters, digits and hyphens",
            ),
            (
                "a_b.localhost",
                "labels may only hold lowercase letters, digits and hyphens",
            ),
            (
                "a b.localhost",
                "labels may only hold lowercase letters, digits and hyphens",
            ),
            (
                "a\t.localhost",
                "labels may only hold lowercase letters, digits and hyphens",
            ),
            (
                "a#.localhost",
                "labels may only hold lowercase letters, digits and hyphens",
            ),
            (
                "café.localhost",
                "labels may only hold lowercase letters, digits and hyphens",
            ),
            ("127.0.0.1", "it is not under an allowed suffix"),
            (
                "::1",
                "labels may only hold lowercase letters, digits and hyphens",
            ),
            ("a.localhost.", "it has an empty label"),
            (".a.localhost", "it has an empty label"),
            ("a..localhost", "it has an empty label"),
            ("-a.localhost", "a label starts or ends with a hyphen"),
            ("a-.localhost", "a label starts or ends with a hyphen"),
            (&long_label, "a label is longer than 63 characters"),
        ];
        for (name, reason) in rejected {
            let error = validate_name(name, &test).expect_err(name);
            assert_eq!(error.reason, *reason, "{name:?}");
        }
    }

    #[test]
    fn total_length_is_capped() {
        let label = "a".repeat(63);
        // Three 63-byte labels with dots (192) + 51 + ".localhost" (10) = 253.
        let fits = format!("{label}.{label}.{label}.{}.localhost", "b".repeat(51));
        assert_eq!(fits.len(), 253);
        assert_eq!(validate_name(&fits, &localhost_only()), Ok(()));
        let too_long = format!("c{fits}");
        assert_eq!(
            validate_name(&too_long, &localhost_only())
                .expect_err("254 bytes")
                .reason,
            "it is longer than 253 characters"
        );
    }

    #[test]
    fn tld_validation_table() {
        for tld in [
            "localhost",
            "test",
            "acme.test",
            "internal",
            "dev.internal",
            "home.arpa",
        ] {
            assert_eq!(validate_tld(tld).as_deref(), Ok(tld), "{tld}");
        }
        for tld in [
            "com", "dev", "local", "lan", "arpa", "test.com", "Test", "test.", "*.test", "",
        ] {
            assert!(validate_tld(tld).is_err(), "{tld:?} must be refused");
        }
    }

    #[test]
    fn suffixes_include_localhost_and_tld() {
        assert_eq!(localhost_only().as_slice(), ["localhost"]);
        let localhost = Suffixes::new(Some("localhost")).expect("localhost");
        assert_eq!(localhost.as_slice(), ["localhost"]);
        let internal = Suffixes::new(Some("dev.internal")).expect("internal");
        assert_eq!(internal.as_slice(), ["localhost", "dev.internal"]);
        assert!(internal.allows("a.dev.internal"));
        assert!(!internal.allows("dev.internal"));
        assert!(!internal.allows("a.xdev.internal"));
        assert!(Suffixes::new(Some("com")).is_err());
    }

    #[test]
    fn parses_lines_and_dedupes() {
        let names = parse_names(
            b"b.localhost\r\na.localhost\n\nb.localhost\n",
            &localhost_only(),
        )
        .expect("valid input");
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            ["a.localhost", "b.localhost"]
        );
        assert_eq!(parse_names(b"", &localhost_only()), Ok(BTreeSet::new()));
    }

    #[test]
    fn one_bad_line_fails_everything() {
        let error = parse_names(b"a.localhost\n evil.localhost\n", &localhost_only())
            .expect_err("leading space");
        assert!(matches!(error, InputError::Name { line: 2, .. }), "{error}");
        assert_eq!(
            parse_names(b"a.localhost\n\xff\n", &localhost_only()),
            Err(InputError::NotUtf8)
        );
    }

    #[test]
    fn count_cap() {
        let at_cap: String = (0..MAX_NAMES)
            .map(|i| format!("n{i}.localhost\n"))
            .collect();
        assert_eq!(
            parse_names(at_cap.as_bytes(), &localhost_only()).map(|names| names.len()),
            Ok(MAX_NAMES)
        );
        let over = format!("{at_cap}extra.localhost\n");
        assert_eq!(
            parse_names(over.as_bytes(), &localhost_only()),
            Err(InputError::TooMany {
                count: MAX_NAMES + 1
            })
        );
        // Repeats of one name do not count against the cap.
        let repeats = "a.localhost\n".repeat(MAX_NAMES + 10);
        assert!(parse_names(repeats.as_bytes(), &localhost_only()).is_ok());
    }

    #[test]
    fn input_size_cap() {
        let huge = vec![b'\n'; MAX_INPUT_BYTES + 1];
        assert_eq!(
            parse_names(&huge, &localhost_only()),
            Err(InputError::TooLarge)
        );
    }
}
