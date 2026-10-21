use std::path::PathBuf;

use crate::{Note, SourceKind};

/// Why a migration could not produce a `devlanes.yaml`.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// A source file could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A source file is not valid for its tool.
    #[error("cannot parse {file}: {message}")]
    Parse { file: String, message: String },
    /// Two sources define a unit with the same name and different settings.
    #[error("{} conflict: {}", sources_text(.sources), describe_conflicts(.units))]
    Conflict {
        /// The sources that were being merged.
        sources: Vec<SourceKind>,
        /// Each unit name defined by more than one source, with those sources.
        units: Vec<(String, Vec<SourceKind>)>,
    },
    /// The sources were read but define nothing devlanes can run.
    #[error("{} define no process devlanes can run", sources_text(.sources))]
    Empty {
        sources: Vec<SourceKind>,
        /// Why each process was skipped.
        notes: Vec<Note>,
    },
    /// The generated configuration failed devlanes validation; nothing is written.
    #[error("the generated devlanes.yaml is invalid: {message}")]
    Invalid { message: String, yaml: String },
}

/// `mprocs and Procfile`, for messages.
fn sources_text(sources: &[SourceKind]) -> String {
    let names: Vec<&str> = sources.iter().map(|kind| kind.name()).collect();
    match names.as_slice() {
        [] => "the sources".to_owned(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `web (mprocs, procfile), worker (mprocs, pm2)`.
fn describe_conflicts(units: &[(String, Vec<SourceKind>)]) -> String {
    units
        .iter()
        .map(|(name, kinds)| {
            let kinds: Vec<&str> = kinds.iter().map(|kind| kind.name()).collect();
            format!("{name} ({})", kinds.join(", "))
        })
        .collect::<Vec<_>>()
        .join(", ")
}
