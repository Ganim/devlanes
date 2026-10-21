use miette::{Diagnostic, NamedSource, SourceSpan};
use serde::Serialize;
use std::path::Path;

/// Location in the original YAML, using one-based line and column numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SourceLocation {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

/// A source-labelled parsing, merge, expression, or validation failure.
#[derive(Debug, thiserror::Error, Diagnostic)]
#[error("{message}")]
pub struct ConfigError {
    pub message: String,
    pub location: SourceLocation,
    #[source_code]
    source_code: std::sync::Arc<NamedSource<String>>,
    #[label("{message}")]
    span: SourceSpan,
}

impl ConfigError {
    /// Attach a diagnostic to a byte position in a source document.
    pub(crate) fn at(file: &str, source: &str, offset: usize, message: impl Into<String>) -> Self {
        let offset = offset.min(source.len());
        let prefix = &source[..source.floor_char_boundary(offset)];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
        let column = prefix
            .rsplit('\n')
            .next()
            .map_or(1, |s| s.chars().count() + 1);
        Self {
            message: message.into(),
            location: SourceLocation {
                file: file.into(),
                line,
                column,
            },
            source_code: std::sync::Arc::new(NamedSource::new(file, source.into())),
            span: (offset, usize::from(offset < source.len())).into(),
        }
    }

    /// Create a diagnostic for errors that do not originate in a YAML document.
    pub(crate) fn plain(message: impl Into<String>) -> Self {
        Self::at("configuration", "", 0, message)
    }

    /// Retain the file name when a configuration cannot be read.
    pub(crate) fn io(path: &Path, error: std::io::Error) -> Self {
        Self::at(&path.display().to_string(), "", 0, error.to_string())
    }
}
