//! The `resolver` hostname strategy: an embedded DNS server answering the user's
//! TLD with loopback addresses, and lane domains under that TLD instead of
//! `.localhost`. Installing the OS resolver file is the CLI's job (it needs sudo).

use std::sync::Arc;

use devlanes_config::{Resolver, Templated};
use devlanes_resolver_dns::{DnsServer, Zone};
use serde_json::{Value, json};

use crate::{
    engine::Inner,
    model::{ConfigState, lock},
};

impl Inner {
    /// The TLD when the user config selects the `resolver` strategy.
    pub(crate) fn resolver_tld(&self) -> Option<String> {
        let hostnames = &self.opts.user.hostnames;
        (hostnames.resolver == Resolver::Resolver)
            .then(|| hostnames.tld.clone())
            .flatten()
            .filter(|tld| !tld.is_empty())
    }

    /// The resolver TLD when it passes [`Zone::new`]'s special-use check; anything
    /// else is reported by `doctor` (through `start_dns`) and otherwise ignored.
    pub(crate) fn resolver_zone(&self) -> Option<Zone> {
        self.resolver_tld().and_then(|tld| Zone::new(&tld).ok())
    }

    /// Makes `<project>[-N].<tld>` the default `lane.domain` under the resolver
    /// strategy. An explicit `lanes.vars.domain` or a `matrix` keeps its own value.
    /// An invalid TLD changes nothing, so the raw config value never reaches the
    /// template expression.
    pub(crate) fn with_resolver_domain(&self, state: ConfigState) -> ConfigState {
        let (ConfigState::Loaded(config), Some(zone)) = (&state, self.resolver_zone()) else {
            return state;
        };
        let tld = zone.name();
        if config.lanes.matrix.is_some() || config.lanes.vars.contains_key("domain") {
            return state;
        }
        let mut adapted = config.as_ref().clone();
        // First, so other vars that read `lane.domain` see this value.
        adapted.lanes.vars.shift_insert(
            0,
            "domain".into(),
            Templated::Expression(format!(
                "${{{{ project.name if lane.index == 0 else project.name ~ '-' ~ lane.index }}}}.{tld}"
            )),
        );
        ConfigState::Loaded(Arc::new(adapted))
    }

    /// Starts the DNS server when the resolver strategy is configured. Failures are
    /// kept for `doctor`; hostnames then only resolve where the OS already can.
    pub(crate) async fn start_dns(self: &Arc<Self>) {
        let Some(tld) = self.resolver_tld() else {
            return;
        };
        let listen = self.opts.dns_listen;
        let status = match Zone::new(&tld) {
            Err(error) => {
                json!({"tld": tld, "listen": listen, "running": false, "error": error.to_string()})
            }
            Ok(zone) => match DnsServer::bind(listen, zone).await {
                Ok(server) => {
                    let bound = server.local_addr().unwrap_or(listen);
                    tokio::spawn(server.serve(self.shutdown.child_token()));
                    json!({"tld": tld, "listen": bound, "running": true, "error": null})
                }
                Err(error) => {
                    tracing::warn!(%error, "DNS server not started");
                    json!({"tld": tld, "listen": listen, "running": false, "error": error.to_string()})
                }
            },
        };
        *lock(&self.dns) = status;
    }

    /// `doctor` facts: the server plus whether the OS resolver file routes the TLD
    /// to it. `null` without the resolver strategy.
    pub(crate) async fn dns_doctor(&self) -> Value {
        let mut status = lock(&self.dns).clone();
        let Some(tld) = self.resolver_tld() else {
            return status;
        };
        let listen = status
            .get("listen")
            .and_then(|l| serde_json::from_value(l.clone()).ok())
            .unwrap_or(self.opts.dns_listen);
        let file = match self.integ.resolver_files.clone() {
            Some(files) => tokio::task::spawn_blocking(move || files.status(&tld, listen))
                .await
                .unwrap_or(Value::Null),
            None => Value::Null,
        };
        if let Some(map) = status.as_object_mut() {
            map.insert("resolver_file".into(), file);
        }
        status
    }
}
