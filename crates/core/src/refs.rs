//! Pure parsing and resolution of CLI lane and unit references.

use crate::{LaneId, ProjectId};
use serde::{Deserialize, Serialize};
use std::{
    path::{Component, Path, PathBuf},
    str::FromStr,
};

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
            || selector.contains(['@', ':'])
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
        let (lane, selector) = match input.split_once(':') {
            Some((lane, unit)) => (Some(lane.parse()?), unit),
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
                    .is_some_and(|path| normalize(&lane.path) == *path),
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
                    && context
                        .lane
                        .map_or_else(|| context.cwd.starts_with(&lane.path), |id| id == lane.id)
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
    let path = if selector.starts_with('/') {
        PathBuf::from(selector)
    } else if selector.starts_with("./") {
        context.cwd.join(selector)
    } else if selector == "~" {
        context.home.clone()?
    } else {
        context.home.as_ref()?.join(selector.strip_prefix("~/")?)
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
