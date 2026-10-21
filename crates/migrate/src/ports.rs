//! Literal ports: each lane needs its own, so a service's fixed port becomes `self.port`
//! when there is exactly one candidate, and other units reach it through `svc.<name>.port`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::draft::{Check, Command, Ready, Unit};

/// A port option, an inline `PORT=` or a local address in a command line. Group 1 is the
/// number.
static SHELL_PORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:(?:^|\s)--port(?:=|\s+)|(?:^|\s)-p\s*|\bPORT=|\b(?:localhost|127\.0\.0\.1|0\.0\.0\.0):)(\d{2,5})\b",
    )
    .expect("valid regex")
});

/// A local address with a port. Group 1 is the host, group 2 the number.
static LOCAL_ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(localhost|127\.0\.0\.1|0\.0\.0\.0):(\d{2,5})\b").expect("valid regex")
});

/// The expression for the service's own port.
const SELF_PORT: &str = "${{ self.port }}";

/// Development servers listen above the privileged range; smaller numbers after `-p` are
/// usually something else (parallelism, a count).
fn plausible(port: u32) -> bool {
    (1024..=65535).contains(&port)
}

/// Fixes ports for every service and rewrites references between units. Notes go on the
/// units they concern.
pub(crate) fn adapt(services: &mut [(String, Unit)], tasks: &mut [(String, Unit)]) {
    let mut owners: BTreeMap<u16, Vec<String>> = BTreeMap::new();
    for (name, unit) in services.iter_mut() {
        if let Some(port) = own_port(unit) {
            owners.entry(port).or_default().push(name.clone());
        }
    }
    let owners: BTreeMap<u16, String> = owners
        .into_iter()
        .filter_map(|(port, names)| match names.as_slice() {
            [one] => Some((port, one.clone())),
            _ => None,
        })
        .collect();
    for (name, unit) in services.iter_mut().chain(tasks.iter_mut()) {
        link_other_services(name, unit, &owners);
    }
}

/// Finds the one literal port a service listens on, rewrites it to `self.port`, drops a
/// literal `PORT` (devlanes sets it) and settles port-based readiness. Returns the port it
/// replaced.
fn own_port(unit: &mut Unit) -> Option<u16> {
    let mut candidates = BTreeSet::new();
    let mut env_port = None;
    if let Some(index) = unit.env.iter().position(|(key, _)| key == "PORT") {
        let value = unit.env[index].1.clone();
        if value.contains("${{") {
            // An expression the user wrote on purpose; devlanes checks it.
        } else if let Some(port) = value.trim().parse::<u16>().ok().filter(|p| *p > 0) {
            candidates.insert(port);
            env_port = Some(index);
        } else {
            unit.env.remove(index);
            unit.note(format!(
                "dropped PORT={value:?}: devlanes sets PORT to the lane's port for each service"
            ));
        }
    }
    for port in command_ports(&unit.run) {
        candidates.insert(port);
    }
    let own = match candidates.len() {
        1 => candidates.first().copied(),
        0 => None,
        _ => {
            let list: Vec<String> = candidates.iter().map(u16::to_string).collect();
            unit.note(format!(
                "several literal ports ({}): use ${{{{ self.port }}}} for the one this service listens on",
                list.join(", ")
            ));
            None
        }
    };
    if let Some(port) = own {
        if let Some(index) = env_port {
            unit.env.remove(index);
        }
        replace_own_port(&mut unit.run, port);
        if let Some(Ready {
            check: Check::Cmd(command),
            ..
        }) = &mut unit.ready
        {
            replace_own_port(command, port);
        }
        unit.note(format!(
            "port {port} is now the lane's port (${{{{ self.port }}}}, also in PORT), so lanes do not collide"
        ));
    }
    settle_probe(unit, own);
    own
}

/// Ports a command line names for the process itself.
fn command_ports(run: &Command) -> Vec<u16> {
    let texts: Vec<&str> = match run {
        Command::Shell(text) => vec![text.as_str()],
        Command::Argv(args) => args.iter().map(String::as_str).collect(),
        Command::Script { .. } | Command::Steps(_) => Vec::new(),
    };
    let mut ports = Vec::new();
    for (index, text) in texts.iter().enumerate() {
        for captures in SHELL_PORT.captures_iter(text) {
            if let Some(port) = parse_port(&captures[1]) {
                ports.push(port);
            }
        }
        // `--port 3000` as two argv items.
        if matches!(run, Command::Argv(_))
            && matches!(*text, "--port" | "-p")
            && let Some(port) = texts.get(index + 1).and_then(|next| parse_port(next))
        {
            ports.push(port);
        }
    }
    ports
}

/// A plausible port number.
fn parse_port(text: &str) -> Option<u16> {
    text.parse::<u32>()
        .ok()
        .filter(|port| plausible(*port))
        .and_then(|port| u16::try_from(port).ok())
}

/// Replaces every mention of `port` in the command's port positions with `self.port`.
fn replace_own_port(run: &mut Command, port: u16) {
    match run {
        Command::Shell(text) => *text = replace_in(text, port),
        Command::Argv(args) => {
            let mut after_flag = false;
            for arg in args.iter_mut() {
                if after_flag && parse_port(arg) == Some(port) {
                    *arg = SELF_PORT.to_owned();
                } else {
                    *arg = replace_in(arg, port);
                }
                after_flag = matches!(arg.as_str(), "--port" | "-p");
            }
        }
        Command::Script { .. } | Command::Steps(_) => {}
    }
}

/// Replaces the number in each [`SHELL_PORT`] match that equals `port`.
fn replace_in(text: &str, port: u16) -> String {
    let mut out = String::new();
    let mut last = 0;
    for captures in SHELL_PORT.captures_iter(text) {
        let number = captures.get(1).expect("group 1 always matches");
        if parse_port(number.as_str()) == Some(port) {
            out.push_str(&text[last..number.start()]);
            out.push_str(SELF_PORT);
            last = number.end();
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Turns a probe on a literal port into `tcp: true` or `http: {path}` when that port is the
/// service's own, into a `curl` command for another HTTP port, and drops it otherwise.
fn settle_probe(unit: &mut Unit, own: Option<u16>) {
    let Some(Ready {
        check: Check::Probe { port, http },
        timeout,
    }) = unit.ready.clone()
    else {
        return;
    };
    let check = if own == Some(port) {
        Some(match http {
            Some(http) => Check::Http {
                path: http.path,
                status: http.status,
            },
            None => Check::Tcp,
        })
    } else if let Some(http) = http {
        unit.note(format!(
            "readiness probes {} on a fixed port; it is the same in every lane",
            http.url
        ));
        Some(Check::Cmd(Command::Shell(format!(
            "curl -fsS -o /dev/null {}",
            crate::words::quote(&http.url)
        ))))
    } else {
        unit.note(format!(
            "dropped the TCP readiness check on port {port}: it is not the port this service gets from devlanes"
        ));
        None
    };
    unit.ready = check.map(|check| Ready { check, timeout });
}

/// Rewrites `localhost:<port>` of another service to `svc.<name>.port` in env values and
/// commands, and notes the local addresses left as they were.
fn link_other_services(name: &str, unit: &mut Unit, owners: &BTreeMap<u16, String>) {
    let mut linked = BTreeSet::new();
    let mut fixed = BTreeSet::new();
    let mut rewrite = |text: &str| -> String {
        let mut out = String::new();
        let mut last = 0;
        for captures in LOCAL_ADDRESS.captures_iter(text) {
            let number = captures.get(2).expect("group 2 always matches");
            let Some(port) = parse_port(number.as_str()) else {
                continue;
            };
            match owners.get(&port) {
                Some(owner) => {
                    out.push_str(&text[last..number.start()]);
                    if owner == name {
                        out.push_str(SELF_PORT);
                    } else {
                        out.push_str(&format!("${{{{ svc.{}.port }}}}", owner.replace('-', "_")));
                        linked.insert(format!("{port} ({owner})"));
                    }
                    last = number.end();
                }
                None => {
                    fixed.insert(port);
                }
            }
        }
        out.push_str(&text[last..]);
        out
    };
    for (_, value) in unit.env.iter_mut() {
        *value = rewrite(value);
    }
    let mut commands = vec![&mut unit.run];
    if let Some(Ready {
        check: Check::Cmd(command),
        ..
    }) = &mut unit.ready
    {
        commands.push(command);
    }
    for command in commands {
        match command {
            Command::Shell(text) => *text = rewrite(text),
            Command::Argv(args) => {
                for arg in args.iter_mut() {
                    *arg = rewrite(arg);
                }
            }
            Command::Script { .. } | Command::Steps(_) => {}
        }
    }
    if !linked.is_empty() {
        let list: Vec<String> = linked.into_iter().collect();
        unit.note(format!(
            "local addresses of other services now use their lane ports: {}",
            list.join(", ")
        ));
    }
    if !fixed.is_empty() {
        let list: Vec<String> = fixed.iter().map(u16::to_string).collect();
        unit.note(format!(
            "fixed local ports remain ({}); they are the same in every lane",
            list.join(", ")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_replaces_port_options() {
        let run = Command::Shell("vite --port 5173 --host 127.0.0.1".into());
        assert_eq!(command_ports(&run), [5173]);
        assert_eq!(
            replace_in("rails s -p 3000 -b 0.0.0.0", 3000),
            "rails s -p ${{ self.port }} -b 0.0.0.0"
        );
        assert_eq!(
            replace_in("PORT=4000 node server.js", 4000),
            "PORT=${{ self.port }} node server.js"
        );
        assert!(command_ports(&Command::Shell("make -p 4".into())).is_empty());
    }
}
