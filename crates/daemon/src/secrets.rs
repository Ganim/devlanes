//! Per-worktree secrets cache in front of the injected provider. `devlanes unlock`
//! fills it with values the client obtained in its own terminal. Values live in
//! daemon memory only and are never logged or persisted.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use devlanes_core::contrib::{SecretEnv, SecretsError, SecretsProvider, SecretsRequest};

/// Provider answers are reused briefly so one start does not run fnox per profile twice.
const PROVIDER_TTL: Duration = Duration::from_secs(60);

/// Cached variables; deliberately neither `Debug` nor serializable.
#[derive(Clone, Default)]
struct Values {
    set: BTreeMap<String, String>,
    remove: BTreeSet<String>,
    files: BTreeMap<String, String>,
}

impl Values {
    /// Copies a provider answer.
    fn from_env(env: &SecretEnv) -> Self {
        Self {
            set: env.set.clone(),
            remove: env.remove.clone(),
            files: env.files.clone(),
        }
    }

    /// Rebuilds the provider type for resolution.
    fn to_env(&self) -> SecretEnv {
        SecretEnv {
            set: self.set.clone(),
            remove: self.remove.clone(),
            files: self.files.clone(),
        }
    }
}

/// Cache entry origin: unlocked values never expire; provider values do.
struct Entry {
    values: Values,
    fetched: Option<Instant>,
}

type Key = (PathBuf, Option<String>);

/// Wraps the configured provider. Without one, only unlocked values are served.
pub struct SecretsCache {
    provider: Option<Arc<dyn SecretsProvider>>,
    entries: Mutex<HashMap<Key, Entry>>,
}

impl SecretsCache {
    /// Caches in front of `provider`.
    pub fn new(provider: Option<Arc<dyn SecretsProvider>>) -> Self {
        Self {
            provider,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Stores values handed over by `devlanes unlock` for a worktree and profile.
    /// `None` also serves every profile of that worktree without its own entry.
    pub fn unlock(
        &self,
        worktree: PathBuf,
        profile: Option<String>,
        set: BTreeMap<String, String>,
        remove: BTreeSet<String>,
    ) -> usize {
        let count = set.len();
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                (worktree, profile),
                Entry {
                    values: Values {
                        set,
                        remove,
                        ..Values::default()
                    },
                    fetched: None,
                },
            );
        }
        count
    }

    /// Drops everything for a worktree (lane clean) or everything (`None`).
    pub fn forget(&self, worktree: Option<&PathBuf>) {
        if let Ok(mut entries) = self.entries.lock() {
            match worktree {
                Some(path) => entries.retain(|(tree, _), _| tree != path),
                None => entries.clear(),
            }
        }
    }

    /// Returns a cached answer when it is still valid.
    fn cached(&self, key: &Key) -> Option<Values> {
        let entries = self.entries.lock().ok()?;
        let fresh = |entry: &Entry| entry.fetched.is_none_or(|at| at.elapsed() < PROVIDER_TTL);
        // Other profiles never fall back to these values, so a locked per-service
        // profile keeps reporting `secrets_locked` until it is unlocked itself.
        entries
            .get(key)
            .filter(|e| fresh(e))
            .map(|entry| entry.values.clone())
    }
}

#[async_trait]
impl SecretsProvider for SecretsCache {
    /// Reports the wrapped provider's id.
    fn id(&self) -> &'static str {
        self.provider.as_ref().map_or("unlock", |p| p.id())
    }

    /// Serves unlocked or recent values, else asks the provider and caches the answer.
    async fn env(&self, request: &SecretsRequest<'_>) -> Result<SecretEnv, SecretsError> {
        let key = (
            request.worktree.to_path_buf(),
            request.profile.map(str::to_owned),
        );
        if let Some(values) = self.cached(&key) {
            return Ok(values.to_env());
        }
        let Some(provider) = &self.provider else {
            return Ok(SecretEnv::default());
        };
        let env = provider.env(request).await?;
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                key,
                Entry {
                    values: Values::from_env(&env),
                    fetched: Some(Instant::now()),
                },
            );
        }
        Ok(env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider that is always locked.
    struct Locked;

    #[async_trait]
    impl SecretsProvider for Locked {
        fn id(&self) -> &'static str {
            "locked"
        }
        async fn env(&self, _: &SecretsRequest<'_>) -> Result<SecretEnv, SecretsError> {
            Err(SecretsError::Locked {
                message: "vault".into(),
            })
        }
    }

    #[tokio::test]
    /// Unlocked values bypass a locked provider for their profile only.
    async fn unlock_serves_locked_provider() {
        let cache = SecretsCache::new(Some(Arc::new(Locked)));
        let tree = PathBuf::from("/w");
        let base = BTreeMap::new();
        let request = SecretsRequest {
            worktree: &tree,
            profile: Some("dev"),
            explicit: true,
            base_env: &base,
        };
        assert!(cache.env(&request).await.is_err());
        cache.unlock(
            tree.clone(),
            Some("dev".into()),
            BTreeMap::from([("K".into(), "v".into())]),
            BTreeSet::new(),
        );
        let env = cache.env(&request).await.expect("unlocked");
        assert_eq!(env.set["K"], "v");
        let other = SecretsRequest {
            profile: Some("admin"),
            ..request
        };
        assert!(cache.env(&other).await.is_err());
    }
}
