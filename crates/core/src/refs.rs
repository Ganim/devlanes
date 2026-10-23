//! Pure parsing and resolution of CLI lane and unit references.

use crate::{
    LaneId, ProjectId,
    paths::{path_starts_with, same_path},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Component, Path, PathBuf},
    str::FromStr,
};

/// Lane path spellings a selector may use. Unix knows `/`, `./` and `~/`;
/// Windows adds drive letters, UNC paths and `\`, whose `C:` colon must not
/// be read as the `lane:unit` separator.
#[cfg(unix)]
mod os {
    /// Bytes at the start of `input` that belong to a drive prefix: none on Unix.
    pub(super) fn drive_prefix_len(_input: &str) -> usize {
        0
    }

    /// Where the search for the `lane:unit` colon starts.
    pub(super) fn unit_separator_start(_input: &str) -> usize {
        0
    }

    /// Absolute lane path forms.
    pub(super) fn is_absolute(selector: &str) -> bool {
        selector.starts_with('/')
    }

    /// Cwd-relative lane path forms.
    pub(super) fn is_relative(selector: &str) -> bool {
        selector.starts_with("./")
    }

    /// The part of a `~/...` selector below the home directory.
    pub(super) fn below_home(selector: &str) -> Option<&str> {
        selector.strip_prefix("~/")
    }
}

#[cfg(windows)]
mod os {
    use std::path::Path;

    /// Length of a leading `C:\` or `C:/`, or of a verbatim `\\?\C:\`, up to
    /// and including the drive colon; else 0. Other forms (`\\server\share`,
    /// `\\?\UNC\`) have no drive colon.
    pub(super) fn drive_prefix_len(input: &str) -> usize {
        match input.as_bytes() {
            [letter, b':', b'\\' | b'/', ..] if letter.is_ascii_alphabetic() => 2,
            [b'\\', b'\\', b'?', b'\\', letter, b':', b'\\', ..]
                if letter.is_ascii_alphabetic() =>
            {
                6
            }
            _ => 0,
        }
    }

    /// Skips a drive prefix at the start or right after `project@`, so
    /// `C:\src\acme:web` and `acme@C:\src\acme:web` split before `web`.
    pub(super) fn unit_separator_start(input: &str) -> usize {
        match drive_prefix_len(input) {
            0 => input
                .split_once('@')
                .map_or(0, |(project, lane)| match drive_prefix_len(lane) {
                    0 => 0,
                    drive => project.len() + 1 + drive,
                }),
            drive => drive,
        }
    }

    /// Absolute lane path forms: `/` as on Unix, plus drive and UNC paths.
    pub(super) fn is_absolute(selector: &str) -> bool {
        selector.starts_with('/') || Path::new(selector).is_absolute()
    }

    /// Cwd-relative lane path forms: `./` as on Unix, plus `.\` and `..\`.
    pub(super) fn is_relative(selector: &str) -> bool {
        selector.starts_with("./") || selector.starts_with(".\\") || selector.starts_with("..\\")
    }

    /// The part of a `~/...` or `~\...` selector below the home directory.
    pub(super) fn below_home(selector: &str) -> Option<&str> {
        selector
            .strip_prefix("~/")
            .or_else(|| selector.strip_prefix("~\\"))
    }
}

/// Parsed `[project@]lane` input. Numeric selectors never fall back to names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneRef {
    pub project: Option<String>,
    pub selector: String,
}

/// Parsed `[lane-ref:]unit`; selectors can be names, globs, or `+tag`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitRef {
    pub lane: Option<LaneRef>,
    pub selector: String,
}

/// Invalid syntax, absence, or ambiguity. Candidates are pasteable lane references.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReferenceError {
    #[error("invalid reference: {0}")]
    Invalid(String),
    #[error("reference not found: {reference}; candidates: {candidates:?}")]
    NotFound {
        reference: String,
        candidates: Vec<String>,
    },
    #[error("ambiguous reference: {reference}; candidates: {candidates:?}")]
    Ambiguous {
        reference: String,
        candidates: Vec<String>,
    },
}

/// Validates the project-name grammar used by references and config.
pub fn valid_project_name(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

impl FromStr for LaneRef {
    type Err = ReferenceError;

    /// Parses syntax only; branch and path lookup happens against supplied records.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (project, selector) = match input.split_once('@') {
            Some((project, lane)) if valid_project_name(project) => {
                (Some(project.to_owned()), lane)
            }
            Some(_) => return Err(ReferenceError::Invalid(input.into())),
            None => (None, input),
        };
        if selector.is_empty()
            || selector[os::drive_prefix_len(selector)..].contains(['@', ':'])
            || selector.chars().any(char::is_control)
        {
            return Err(ReferenceError::Invalid(input.into()));
        }
        Ok(Self {
            project,
            selector: selector.into(),
        })
    }
}

impl FromStr for UnitRef {
    type Err = ReferenceError;

    /// Retains the unit selector for the caller's name/glob/tag matching.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let start = os::unit_separator_start(input);
        let (lane, selector) = match input[start..].find(':') {
            Some(colon) => (
                Some(input[..start + colon].parse()?),
                &input[start + colon + 1..],
            ),
            None => (None, input),
        };
        if selector.is_empty()
            || selector == "+"
            || selector.contains([':', '@'])
            || selector.chars().any(char::is_whitespace)
        {
            return Err(ReferenceError::Invalid(input.into()));
        }
        Ok(Self {
            lane,
            selector: selector.into(),
        })
    }
}

/// Minimal lane metadata for resolution; no filesystem access is performed.
#[derive(Debug, Clone)]
pub struct LaneCandidate {
    pub id: LaneId,
    pub project_id: ProjectId,
    pub project: String,
    pub index: u32,
    pub branch: Option<String>,
    pub name: String,
    pub path: PathBuf,
}

/// Explicit context takes precedence over context inferred by the caller from cwd.
#[derive(Debug, Default)]
pub struct ResolveContext {
    pub project: Option<ProjectId>,
    pub lane: Option<LaneId>,
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
}

/// Unit metadata for exact, glob, or tag selection within a resolved lane.
#[derive(Debug, Clone)]
pub struct UnitCandidate {
    pub lane_id: LaneId,
    pub name: String,
    pub tags: Vec<String>,
}

/// Resolves a name/glob/`+tag` selector within one lane. Results are sorted by name;
/// no match is an error with pasteable candidates. Singular commands should reject
/// multiple results rather than choosing one arbitrarily.
pub fn resolve_units<'a>(
    reference: &UnitRef,
    lanes: &[LaneCandidate],
    units: &'a [UnitCandidate],
    context: &ResolveContext,
) -> Result<Vec<&'a UnitCandidate>, ReferenceError> {
    let (lane, selector) = resolve_unit(reference, lanes, context)?;
    let eligible: Vec<_> = units
        .iter()
        .filter(|unit| unit.lane_id == lane.id)
        .collect();
    let mut matches: Vec<_> = if let Some(tag) = selector.strip_prefix('+') {
        eligible
            .iter()
            .copied()
            .filter(|unit| unit.tags.iter().any(|value| value == tag))
            .collect()
    } else {
        let pattern = globset::Glob::new(selector)
            .map_err(|_| ReferenceError::Invalid(selector.into()))?
            .compile_matcher();
        eligible
            .iter()
            .copied()
            .filter(|unit| pattern.is_match(&unit.name))
            .collect()
    };
    matches.sort_by(|a, b| a.name.cmp(&b.name));
    if matches.is_empty() {
        let mut candidates: Vec<_> = eligible
            .iter()
            .map(|unit| format!("{}@{}:{}", lane.project, lane.index, unit.name))
            .collect();
        candidates.sort();
        return Err(ReferenceError::NotFound {
            reference: selector.into(),
            candidates,
        });
    }
    Ok(matches)
}

/// Resolves by number, branch, lane name, directory name, then lexical path.
/// Paths supplied by the caller should already use a consistent absolute spelling.
pub fn resolve_lane<'a>(
    reference: &LaneRef,
    lanes: &'a [LaneCandidate],
    context: &ResolveContext,
) -> Result<&'a LaneCandidate, ReferenceError> {
    let eligible: Vec<_> = lanes
        .iter()
        .filter(|lane| match &reference.project {
            Some(project) => &lane.project == project,
            None => context
                .project
                .is_none_or(|project| lane.project_id == project),
        })
        .collect();
    let selector = reference.selector.as_str();
    let numeric = selector.bytes().all(|b| b.is_ascii_digit());
    let path = expand_path(selector, context);
    let ranks = if numeric { 0..1 } else { 1..5 };
    for rank in ranks {
        let matches: Vec<_> = eligible
            .iter()
            .copied()
            .filter(|lane| match rank {
                0 => selector.parse::<u32>().ok() == Some(lane.index),
                1 => lane.branch.as_deref() == Some(selector),
                2 => lane.name == selector,
                3 => lane.path.file_name().is_some_and(|name| name == selector),
                _ => path
                    .as_ref()
                    .is_some_and(|path| same_path(&normalize(&lane.path), path)),
            })
            .collect();
        match matches.as_slice() {
            [lane] => return Ok(lane),
            [] => {}
            _ => {
                return Err(ReferenceError::Ambiguous {
                    reference: selector.into(),
                    candidates: candidate_names(&matches),
                });
            }
        }
    }
    Err(ReferenceError::NotFound {
        reference: selector.into(),
        candidates: candidate_names(&eligible),
    })
}

/// Resolves a unit's lane, using an explicit/current lane for unqualified units.
/// Returns the untouched unit selector alongside the selected lane.
pub fn resolve_unit<'a, 'r>(
    reference: &'r UnitRef,
    lanes: &'a [LaneCandidate],
    context: &ResolveContext,
) -> Result<(&'a LaneCandidate, &'r str), ReferenceError> {
    let lane = if let Some(lane) = &reference.lane {
        resolve_lane(lane, lanes, context)?
    } else {
        let matches: Vec<_> = lanes
            .iter()
            .filter(|lane| {
                context
                    .project
                    .is_none_or(|project| project == lane.project_id)
                    && context.lane.map_or_else(
                        || path_starts_with(&context.cwd, &lane.path),
                        |id| id == lane.id,
                    )
            })
            .collect();
        match matches.as_slice() {
            [lane] => *lane,
            [] => {
                return Err(ReferenceError::NotFound {
                    reference: reference.selector.clone(),
                    candidates: candidate_names(&lanes.iter().collect::<Vec<_>>()),
                });
            }
            _ => {
                return Err(ReferenceError::Ambiguous {
                    reference: reference.selector.clone(),
                    candidates: candidate_names(&matches),
                });
            }
        }
    };
    Ok((lane, &reference.selector))
}

/// Produces stable candidate ordering without depending on database result order.
fn candidate_names(lanes: &[&LaneCandidate]) -> Vec<String> {
    let mut names: Vec<_> = lanes
        .iter()
        .map(|lane| format!("{}@{}", lane.project, lane.index))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Expands only documented path forms without reading the host environment.
fn expand_path(selector: &str, context: &ResolveContext) -> Option<PathBuf> {
    let path = if os::is_absolute(selector) {
        PathBuf::from(selector)
    } else if os::is_relative(selector) {
        context.cwd.join(selector)
    } else if selector == "~" {
        context.home.clone()?
    } else {
        context.home.as_ref()?.join(os::below_home(selector)?)
    };
    Some(normalize(&path))
}

/// Normalizes dot components lexically; symlink canonicalization belongs to callers.
fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            component => result.push(component.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constructs candidates whose path basename matches the branch.
    fn lane(index: u32, branch: &str, name: &str) -> LaneCandidate {
        LaneCandidate {
            id: LaneId(i64::from(index)),
            project_id: ProjectId(1),
            project: "demo".into(),
            index,
            branch: Some(branch.into()),
            name: name.into(),
            path: PathBuf::from(format!("/work/{branch}")),
        }
    }

    #[test]
    /// Numeric and branch selectors take precedence over display names.
    fn resolution_precedence_and_paths() {
        let lanes = [
            lane(0, "main", "lane0"),
            lane(1, "feature", "main"),
            lane(2, "0", "feature"),
        ];
        let context = ResolveContext {
            cwd: "/work".into(),
            home: Some("/work".into()),
            ..Default::default()
        };
        for (input, expected) in [
            ("0", 0),
            ("main", 0),
            ("feature", 1),
            ("lane0", 0),
            ("./main", 0),
            ("~/feature", 1),
            ("demo@2", 2),
        ] {
            let reference = input.parse().expect("valid reference");
            assert_eq!(
                resolve_lane(&reference, &lanes, &context)
                    .expect("resolved")
                    .index,
                expected
            );
        }
        assert!(resolve_lane(&"99".parse().expect("valid"), &lanes, &context).is_err());
    }

    #[test]
    /// Unscoped ambiguity reports candidates instead of choosing a project.
    fn outside_project_ambiguity_and_invalid_syntax() {
        let mut second = lane(0, "main", "main");
        second.project = "other".into();
        second.project_id = ProjectId(2);
        let lanes = [lane(0, "main", "main"), second];
        assert!(matches!(
            resolve_lane(
                &"0".parse().expect("valid"),
                &lanes,
                &ResolveContext::default()
            ),
            Err(ReferenceError::Ambiguous { .. })
        ));
        for input in ["", "@0", "Demo@0", "demo@", "demo@@1", "0:api"] {
            assert!(input.parse::<LaneRef>().is_err(), "{input}");
        }
        assert_eq!(
            "demo@feat/api:+api"
                .parse::<UnitRef>()
                .expect("valid")
                .selector,
            "+api"
        );
    }

    /// Splits a unit reference into (project, lane selector, unit selector).
    fn unit_parts(input: &str) -> Option<(Option<String>, Option<String>, String)> {
        let reference: UnitRef = input.parse().ok()?;
        let (project, lane) = match reference.lane {
            Some(lane) => (lane.project, Some(lane.selector)),
            None => (None, None),
        };
        Some((project, lane, reference.selector))
    }

    #[test]
    /// POSIX lane paths and plain names split the same way on every OS.
    fn unit_refs_split_on_the_lane_colon() {
        let some = |value: &str| Some(value.to_owned());
        for (input, expected) in [
            ("web", Some((None, None, "web".to_owned()))),
            ("2:web", Some((None, some("2"), "web".into()))),
            ("demo@2:web", Some((some("demo"), some("2"), "web".into()))),
            (
                "/src/acme:web",
                Some((None, some("/src/acme"), "web".into())),
            ),
            ("./acme:+api", Some((None, some("./acme"), "+api".into()))),
            ("~/acme:web", Some((None, some("~/acme"), "web".into()))),
            ("2:web:extra", None),
            ("2:", None),
        ] {
            assert_eq!(unit_parts(input), expected, "{input}");
        }
    }

    #[cfg(unix)]
    #[test]
    /// A drive-letter spelling has no meaning on Unix and stays invalid there.
    fn unix_drive_letters_are_not_paths() {
        assert!(r"C:\src\acme:web".parse::<UnitRef>().is_err());
        assert!(r"C:\src\acme".parse::<LaneRef>().is_err());
    }

    #[cfg(windows)]
    #[test]
    /// The drive colon is part of the lane path, not the `lane:unit` separator.
    fn windows_drive_letters_belong_to_the_lane() {
        let some = |value: &str| Some(value.to_owned());
        for (input, expected) in [
            (
                r"C:\src\acme:web",
                Some((None, some(r"C:\src\acme"), "web".into())),
            ),
            (
                "d:/src/acme:web",
                Some((None, some("d:/src/acme"), "web".into())),
            ),
            (
                r"demo@C:\src\acme:+api",
                Some((some("demo"), some(r"C:\src\acme"), "+api".into())),
            ),
            (r".\acme:web", Some((None, some(r".\acme"), "web".into()))),
            (
                r"\\?\C:\src\acme:web",
                Some((None, some(r"\\?\C:\src\acme"), "web".into())),
            ),
            (
                r"\\server\share\acme:web",
                Some((None, some(r"\\server\share\acme"), "web".into())),
            ),
            (r"C:\src\acme", None),
            (r"C:\src:acme:web", None),
            // Drive-relative `C:web` is not a lane path form: lane `C`, unit `web`.
            ("C:web", Some((None, some("C"), "web".into()))),
        ] {
            assert_eq!(unit_parts(input), expected, "{input}");
        }
        assert!(r"C:\src\acme".parse::<LaneRef>().is_ok());
        assert!(r"C:\src\a:b".parse::<LaneRef>().is_err());
    }

    #[cfg(windows)]
    #[test]
    /// Drive, UNC, verbatim, `.\`, `..\` and `~\` lane paths resolve, ignoring case.
    fn windows_path_forms_resolve() {
        let mut lanes = [lane(0, "main", "main"), lane(1, "feature", "feature")];
        lanes[0].path = PathBuf::from(r"C:\Work Dir\main");
        lanes[1].path = PathBuf::from(r"\\server\share\feature");
        let context = ResolveContext {
            cwd: r"C:\Work Dir\sub".into(),
            home: Some(r"C:\Work Dir".into()),
            ..Default::default()
        };
        for (input, expected) in [
            (r"C:\Work Dir\main", 0),
            ("c:/work dir/MAIN", 0),
            (r".\..\main", 0),
            (r"..\main", 0),
            (r"~\main", 0),
            ("~/main", 0),
            (r"\\?\C:\work dir\Main", 0),
            (r"\\server\share\feature", 1),
            (r"\\?\UNC\SERVER\share\feature", 1),
        ] {
            let reference = input.parse().expect("valid reference");
            assert_eq!(
                resolve_lane(&reference, &lanes, &context)
                    .expect("resolved")
                    .index,
                expected,
                "{input}"
            );
        }
        let inside = ResolveContext {
            cwd: r"c:\work dir\MAIN\src".into(),
            ..Default::default()
        };
        let (lane, _) =
            resolve_unit(&"web".parse().expect("valid"), &lanes, &inside).expect("the cwd's lane");
        assert_eq!(lane.index, 0);
    }

    #[test]
    /// Unit matching stays within the selected lane and preserves deterministic order.
    fn unit_names_globs_tags_and_context() {
        let lanes = [lane(0, "main", "main"), lane(1, "feature", "feature")];
        let units = [
            UnitCandidate {
                lane_id: LaneId(0),
                name: "web-ui".into(),
                tags: vec!["web".into()],
            },
            UnitCandidate {
                lane_id: LaneId(0),
                name: "web-api".into(),
                tags: vec!["api".into()],
            },
            UnitCandidate {
                lane_id: LaneId(1),
                name: "other".into(),
                tags: vec!["api".into()],
            },
        ];
        let context = ResolveContext {
            cwd: "/work/main/subdir".into(),
            ..Default::default()
        };
        for (reference, expected) in [
            ("web-api", vec!["web-api"]),
            ("web-*", vec!["web-api", "web-ui"]),
            ("+api", vec!["web-api"]),
            ("demo@feature:+api", vec!["other"]),
        ] {
            let reference = reference.parse().expect("valid reference");
            let resolved = resolve_units(&reference, &lanes, &units, &context).expect("units");
            assert_eq!(
                resolved
                    .iter()
                    .map(|unit| unit.name.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
        let missing = resolve_units(
            &"missing".parse().expect("reference"),
            &lanes,
            &units,
            &context,
        );
        assert!(
            matches!(missing, Err(ReferenceError::NotFound { candidates, .. }) if candidates == ["demo@0:web-api", "demo@0:web-ui"])
        );
    }
}
