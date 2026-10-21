use std::fmt;

use crate::Error;

/// A `docker compose` release number. Build suffixes such as `-desktop.1` are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComposeVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ComposeVersion {
    /// First release that supports both `!reset` and `!override` in override files.
    pub const OVERRIDE_TAGS: ComposeVersion = ComposeVersion::new(2, 24, 4);

    /// Creates a version from its components.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses `docker compose version --short` output (`2.24.4`, `v2.29.1-desktop.1`,
    /// `5.1.2`). A missing patch component counts as zero.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let trimmed = text.trim();
        let digits = trimmed.strip_prefix('v').unwrap_or(trimmed);
        let core = digits
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .next()
            .unwrap_or_default();
        let mut parts = core.split('.');
        let mut component = |required: bool| -> Result<u32, Error> {
            match parts.next() {
                Some(part) if !part.is_empty() => part
                    .parse()
                    .map_err(|_| Error::VersionParse(text.to_owned())),
                _ if required => Err(Error::VersionParse(text.to_owned())),
                _ => Ok(0),
            }
        };
        Ok(Self::new(
            component(true)?,
            component(true)?,
            component(false)?,
        ))
    }

    /// Fails with a clear upgrade hint when per-lane override files cannot work.
    pub fn require_override_tags(self) -> Result<(), Error> {
        if self < Self::OVERRIDE_TAGS {
            return Err(Error::UnsupportedVersion {
                found: self,
                required: Self::OVERRIDE_TAGS,
            });
        }
        Ok(())
    }
}

impl fmt::Display for ComposeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_formats() {
        let cases = [
            ("5.1.2\n", ComposeVersion::new(5, 1, 2)),
            ("v2.24.4", ComposeVersion::new(2, 24, 4)),
            ("2.29.1-desktop.1", ComposeVersion::new(2, 29, 1)),
            ("2.40.3+ds1-0ubuntu1", ComposeVersion::new(2, 40, 3)),
            ("2.30", ComposeVersion::new(2, 30, 0)),
        ];
        for (text, expected) in cases {
            assert_eq!(ComposeVersion::parse(text).ok(), Some(expected), "{text}");
        }
    }

    #[test]
    fn rejects_garbage() {
        for text in ["", "docker-compose", "v", "2", "x.1.2"] {
            assert!(ComposeVersion::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn override_tags_need_2_24_4() {
        assert!(
            ComposeVersion::new(2, 24, 3)
                .require_override_tags()
                .is_err()
        );
        assert!(
            ComposeVersion::new(2, 24, 4)
                .require_override_tags()
                .is_ok()
        );
        assert!(ComposeVersion::new(5, 0, 0).require_override_tags().is_ok());
        let message = ComposeVersion::new(2, 20, 0)
            .require_override_tags()
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(
            message.contains("2.20.0") && message.contains("2.24.4"),
            "{message}"
        );
    }
}
