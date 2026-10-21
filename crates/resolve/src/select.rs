use devlanes_config::{
    Extensions, ProfileConfig, ProfileOptions, ProjectConfig, ServiceConfig, Templated,
};
use globset::Glob;

use crate::ResolveError;

/// Services to start and the profiles that apply to them.
#[derive(Debug, Clone)]
pub struct Selection {
    /// Service names in declaration order.
    pub services: Vec<String>,
    /// Profiles in effect, in the order given: the requested ones, else
    /// `["default"]` when the config defines it, else empty.
    pub profiles: Vec<String>,
    /// First entry of `profiles`, for single-profile callers.
    pub profile: Option<String>,
    /// Object-form options of the profiles, merged (see [`select_profiles`]).
    pub profile_options: Option<ProfileOptions>,
}

/// One profile as clients list it (TUI picker, `devlanes profiles`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProfileSummary {
    pub name: String,
    /// Selector list, or `include` of the object form.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// `infra: false` written literally. An expression counts as false here; the
    /// lane start evaluates it.
    pub skips_infra: bool,
    /// The profile `devlanes start` uses without `--profile`.
    pub is_default: bool,
}

/// Lists the config's profiles in declaration order. Pure: no lane context.
pub fn profile_summaries<E: Extensions>(config: &ProjectConfig<E>) -> Vec<ProfileSummary> {
    config
        .profiles
        .iter()
        .map(|(name, profile)| {
            let (include, exclude, skips_infra) = match profile {
                ProfileConfig::Selectors(values) => (values.clone(), Vec::new(), false),
                ProfileConfig::Object(options) => (
                    options.include.clone().unwrap_or_default(),
                    options.exclude.clone().unwrap_or_default(),
                    matches!(options.infra, Some(Templated::Literal(false))),
                ),
            };
            ProfileSummary {
                name: name.clone(),
                include,
                exclude,
                skips_infra,
                is_default: name == "default",
            }
        })
        .collect()
}

/// Validates and orders requested profile names: blanks and repeats dropped,
/// `default` allowed even when the config lacks it (it then means "no profile"),
/// and `["default"]` when nothing was requested and the config defines it.
pub(crate) fn active_profiles<E: Extensions, P: AsRef<str>>(
    config: &ProjectConfig<E>,
    requested: &[P],
) -> Result<Vec<String>, ResolveError> {
    let mut names: Vec<String> = Vec::new();
    for name in requested
        .iter()
        .map(|n| n.as_ref().trim())
        .filter(|n| !n.is_empty())
    {
        if !config.profiles.contains_key(name) && name != "default" {
            return Err(ResolveError::UnknownProfile {
                name: name.to_owned(),
                available: config.profiles.keys().cloned().collect(),
            });
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    if names.is_empty() && config.profiles.contains_key("default") {
        names.push("default".to_owned());
    }
    Ok(names)
}

/// Merges the options of several profiles: `env` and `run_scripts` in order
/// (later wins), `infra: false` only when every configured profile says so
/// literally. A single object-form profile is returned unchanged; `None` when no
/// profile has the object form.
pub(crate) fn merge_options<E: Extensions>(
    config: &ProjectConfig<E>,
    names: &[String],
) -> Option<ProfileOptions> {
    let configured: Vec<&ProfileConfig> = names
        .iter()
        .filter_map(|name| config.profiles.get(name))
        .collect();
    let objects: Vec<&ProfileOptions> = configured
        .iter()
        .filter_map(|profile| match profile {
            ProfileConfig::Object(options) => Some(options),
            ProfileConfig::Selectors(_) => None,
        })
        .collect();
    match (configured.len(), objects.as_slice()) {
        (_, []) => None,
        (1, [only]) => Some((*only).clone()),
        _ => {
            let mut merged = ProfileOptions::default();
            for options in &objects {
                merged
                    .env
                    .extend(options.env.iter().map(|(k, v)| (k.clone(), v.clone())));
                merged.run_scripts.extend(
                    options
                        .run_scripts
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone())),
                );
            }
            let all_off = configured.iter().all(|profile| {
                matches!(
                    profile,
                    ProfileConfig::Object(ProfileOptions {
                        infra: Some(Templated::Literal(false)),
                        ..
                    })
                )
            });
            merged.infra = all_off.then_some(Templated::Literal(false));
            Some(merged)
        }
    }
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

/// Single-profile form of [`select_profiles`].
pub fn select<E: Extensions>(
    config: &ProjectConfig<E>,
    profile: Option<&str>,
    selectors: &[String],
) -> Result<Selection, ResolveError> {
    select_profiles(config, profile.as_slice(), selectors)
}

/// Picks the services for `devlanes start` with one or more profiles.
///
/// - `selectors` (names, globs, `+tag`) choose services directly; each must match
///   something. They may name `autostart: false` services. The profiles still
///   contribute their options.
/// - Without selectors, the union of each profile's list (or `include` minus
///   `exclude`) applies, in declaration order. Inside a profile, `autostart:
///   false` services are included only by exact name or `+tag`, so `include: ["*"]`
///   keeps on-demand services stopped.
/// - Without profiles, `default` is used when defined; otherwise every
///   `autostart` service starts.
///
/// `profile_options` merges the profiles: `env` and `run_scripts` in the order
/// given (later wins); `infra: false` only when every profile says so literally.
/// [`crate::resolve_lane`] evaluates templated `infra` values per lane.
pub fn select_profiles<E: Extensions, P: AsRef<str>>(
    config: &ProjectConfig<E>,
    profiles: &[P],
    selectors: &[String],
) -> Result<Selection, ResolveError> {
    let names = active_profiles(config, profiles)?;
    let configured: Vec<&ProfileConfig> = names
        .iter()
        .filter_map(|name| config.profiles.get(name))
        .collect();
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
    } else if !configured.is_empty() {
        let mut result = Vec::new();
        for (name, service) in &config.services {
            let mut keep = false;
            for profile in &configured {
                keep |= profile_selects(profile, name, service)?;
            }
            if keep {
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
        profile: names.first().cloned(),
        profile_options: merge_options(config, &names),
        profiles: names,
    })
}

/// Whether one profile's `include` minus `exclude` keeps `service`.
fn profile_selects<E: Extensions>(
    profile: &ProfileConfig,
    name: &str,
    service: &ServiceConfig<E>,
) -> Result<bool, ResolveError> {
    let (include, exclude): (&[String], &[String]) = match profile {
        ProfileConfig::Selectors(values) => (values, &[]),
        ProfileConfig::Object(options) => (
            options.include.as_deref().unwrap_or_default(),
            options.exclude.as_deref().unwrap_or_default(),
        ),
    };
    let tags = service.tags.as_deref();
    let mut included = Match::No;
    for selector in include {
        match matches(selector, name, tags)? {
            Match::Explicit => included = Match::Explicit,
            Match::Pattern if included == Match::No => included = Match::Pattern,
            _ => {}
        }
    }
    for selector in exclude {
        if matches(selector, name, tags)? != Match::No {
            return Ok(false);
        }
    }
    Ok(match included {
        Match::Explicit => true,
        Match::Pattern => autostart(&service.autostart),
        Match::No => false,
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
