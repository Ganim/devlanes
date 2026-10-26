/// Normalises a routed hostname and checks it against DNS label rules.
///
/// The result is lowercase without a trailing dot. A wildcard is allowed only as a
/// leading `*.` label; every other label is 1-63 letters, digits or inner hyphens,
/// and the whole name stays within 253 characters. The error names the host and
/// the rule it breaks, without a config path.
pub fn normalize_host(host: &str) -> Result<String, String> {
    let trimmed = host.trim();
    let host = trimmed
        .strip_suffix('.')
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    broken_rule(&host).map_or(Ok(host), |why| Err(format!("`{trimmed}`: {why}")))
}

/// Checks the literal parts of a host template, with each `${{ ... }}` standing for
/// one valid label; the evaluated name is checked again when a lane resolves it.
pub(crate) fn check_host_template(template: &str) -> Result<(), String> {
    let mut shape = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("${{") {
        shape.push_str(&rest[..start]);
        shape.push('x');
        rest = rest[start..]
            .find("}}")
            .map_or("", |end| &rest[start + end + 2..]);
    }
    shape.push_str(rest);
    let shape = shape
        .strip_suffix('.')
        .unwrap_or(&shape)
        .to_ascii_lowercase();
    // The message names the template the user wrote, not the stand-in.
    broken_rule(&shape).map_or(Ok(()), |why| Err(format!("`{template}`: {why}")))
}

/// Checks a `ready.http.host` value. It goes into the probe's Host header as written
/// and may carry a port, so only an empty value, spaces and control characters (which
/// would split the request) are refused.
pub fn check_host_header(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("the Host header cannot be empty".into());
    }
    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "`{}`: a Host header cannot contain spaces or control characters",
            host.escape_debug()
        ));
    }
    Ok(())
}

/// The first DNS rule a lowercase, dot-trimmed host breaks.
fn broken_rule(host: &str) -> Option<String> {
    let name = host.strip_prefix("*.").unwrap_or(host);
    if name.is_empty() {
        return Some("not a hostname".into());
    }
    if name.len() > 253 {
        return Some("longer than 253 characters".into());
    }
    for label in name.split('.') {
        if label.contains('*') {
            return Some("a wildcard is only allowed as a leading `*.` label".into());
        }
        if label.is_empty() {
            return Some("empty label between dots".into());
        }
        if label.len() > 63 {
            return Some(format!("label `{label}` is longer than 63 characters"));
        }
        if let Some(bad) = label
            .chars()
            .find(|c| !c.is_ascii_alphanumeric() && *c != '-')
        {
            return Some(format!(
                "`{bad}` is not allowed in a hostname; use letters, digits and hyphens"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Some(format!("label `{label}` cannot start or end with a hyphen"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Case and one trailing dot are normalised; labels follow DNS rules.
    fn hostnames() {
        assert_eq!(
            normalize_host("API.Acme.localhost."),
            Ok("api.acme.localhost".into())
        );
        assert_eq!(
            normalize_host("*.acme.localhost"),
            Ok("*.acme.localhost".into())
        );
        for bad in [
            "api_v2.acme.localhost",
            "a..b",
            "-a.localhost",
            "a-.localhost",
            "api.*.localhost",
            "*",
            "",
            "a b.localhost",
            "api.acme.localhost..",
        ] {
            assert!(normalize_host(bad).is_err(), "{bad}");
        }
        assert!(normalize_host(&format!("{}.localhost", "a".repeat(64))).is_err());
        assert_eq!(
            normalize_host("api_v2.acme.localhost"),
            Err(
                "`api_v2.acme.localhost`: `_` is not allowed in a hostname; use letters, digits and hyphens"
                    .into()
            )
        );
    }

    #[test]
    /// A Host header may carry a port but nothing that would split the request.
    fn host_headers() {
        assert!(check_host_header("api.acme.localhost").is_ok());
        assert!(check_host_header("127.0.0.1:8080").is_ok());
        for bad in ["", "a b", "api\r\nX-Evil: 1", "api\t"] {
            assert!(check_host_header(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    /// Expressions stand for one label; the literal text around them is checked.
    fn templates() {
        assert!(check_host_template("api.${{ lane.domain }}").is_ok());
        assert!(check_host_template("*.${{ lane.domain }}").is_ok());
        assert!(check_host_template("${{ lane.name }}-api.localhost").is_ok());
        let error = check_host_template("api_v2.${{ lane.domain }}").expect_err("underscore");
        assert!(
            error.starts_with("`api_v2.${{ lane.domain }}`: `_`"),
            "{error}"
        );
    }
}
