use devlanes_config::{Extensions, ProfileConfig, ProfileOptions, ProjectConfig, Templated};
use globset::Glob;

use crate::ResolveError;

/// Services to start and the profile that applies to them.
#[derive(Debug, Clone)]
pub struct Selection {
    /// Service names in declaration order.
    pub services: Vec<String>,
    /// Profile in effect: the requested one, else `default` when it exists.
    pub profile: Option<String>,
    /// Object-form options of that profile (env, `infra`, `run_scripts`).
    pub profile_options: Option<ProfileOptions>,
}

/// How a selector matched a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    No,
    /// By exact name or `+tag`.
    Explicit,
    /// By a glob pattern.
    Pattern,
}

/// Picks the services for `devlanes start`.
///
/// - `selectors` (names, globs, `+tag`) choose services directly; each must match
///   something. They may name `autostart: false` services. The profile, if any,
///   still contributes its options.
/// - Without selectors, the profile's list (or `include` minus `exclude`) applies.
///   Inside a profile, `autostart: false` services are included only by exact
///   name or `+tag`, so `include: ["*"]` keeps on-demand services stopped.
/// - Without a profile, `default` is used when defined; otherwise every
///   `autostart` service starts.
pub fn select<E: Extensions>(
    config: &ProjectConfig<E>,
    profile: Option<&str>,
    selectors: &[String],
) -> Result<Selection, ResolveError> {
    let profile_name = match profile {
        Some(name) if config.profiles.contains_key(name) => Some(name.to_owned()),
        Some(name) => {
            return Err(ResolveError::UnknownProfile {
                name: name.to_owned(),
                available: config.profiles.keys().cloned().collect(),
            });
        }
        None => config
            .profiles
            .contains_key("default")
            .then(|| "default".to_owned()),
    };
    let profile_config = profile_name.as_ref().and_then(|p| config.profiles.get(p));
    let profile_options = match profile_config {
        Some(ProfileConfig::Object(options)) => Some(options.clone()),
        _ => None,
    };
    let services = if !selectors.is_empty() {
        let mut chosen = vec![false; config.services.len()];
        for selector in selectors {
            let mut any = false;
            for (index, (name, service)) in config.services.iter().enumerate() {
                if matches(selector, name, service.tags.as_deref())? != Match::No {
                    chosen[index] = true;
                    any = true;
                }
            }
            if !any {
                return Err(ResolveError::NoMatch {
                    selector: selector.clone(),
                    candidates: config.services.keys().cloned().collect(),
                });
            }
        }
        config
            .services
            .keys()
            .zip(chosen)
            .filter(|(_, chosen)| *chosen)
            .map(|(name, _)| name.clone())
            .collect()
    } else if let Some(profile) = profile_config {
        let (include, exclude): (&[String], &[String]) = match profile {
            ProfileConfig::Selectors(values) => (values, &[]),
            ProfileConfig::Object(options) => (
                options.include.as_deref().unwrap_or_default(),
                options.exclude.as_deref().unwrap_or_default(),
            ),
        };
        let mut result = Vec::new();
        for (name, service) in &config.services {
            let tags = service.tags.as_deref();
            let mut included = Match::No;
            for selector in include {
                match matches(selector, name, tags)? {
                    Match::Explicit => included = Match::Explicit,
                    Match::Pattern if included == Match::No => included = Match::Pattern,
                    _ => {}
                }
            }
            let mut excluded = false;
            for selector in exclude {
                excluded |= matches(selector, name, tags)? != Match::No;
            }
            let keep = match included {
                Match::Explicit => true,
                Match::Pattern => autostart(&service.autostart),
                Match::No => false,
            };
            if keep && !excluded {
                result.push(name.clone());
            }
        }
        result
    } else {
        config
            .services
            .iter()
            .filter(|(_, service)| autostart(&service.autostart))
            .map(|(name, _)| name.clone())
            .collect()
    };
    Ok(Selection {
        services,
        profile: profile_name,
        profile_options,
    })
}

/// Literal `autostart`, defaulting to true. Expressions count as true because
/// selection has no lane context.
fn autostart(value: &Option<Templated<bool>>) -> bool {
    !matches!(value, Some(Templated::Literal(false)))
}

/// Matches one selector against a service name and its tags.
fn matches(selector: &str, name: &str, tags: Option<&[String]>) -> Result<Match, ResolveError> {
    if let Some(tag) = selector.strip_prefix('+') {
        let hit = tags.is_some_and(|tags| tags.iter().any(|t| t == tag));
        return Ok(if hit { Match::Explicit } else { Match::No });
    }
    if selector == name {
        return Ok(Match::Explicit);
    }
    if !selector.contains(['*', '?', '[', '{']) {
        return Ok(Match::No);
    }
    let glob = Glob::new(selector).map_err(|e| ResolveError::InvalidSelector {
        selector: selector.to_owned(),
        message: e.to_string(),
    })?;
    Ok(if glob.compile_matcher().is_match(name) {
        Match::Pattern
    } else {
        Match::No
    })
}
