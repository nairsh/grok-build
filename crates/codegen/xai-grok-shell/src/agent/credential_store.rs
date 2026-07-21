//! Provider-independent **credential store**.
//!
//! Credentials — subscription OAuth tokens and saved API keys — are persisted
//! keyed by an arbitrary **credential id**, decoupled from any provider or
//! connection. A `[connection.*]` references a credential by id
//! ([`crate::agent::connection::CredentialRef::Oauth`] / `Named`); many
//! connections can share one credential, and one provider can have many
//! credentials (personal vs. work vs. project). This id-keyed indirection is the
//! concrete realization of "a provider is not an account" and the deliberate
//! step beyond Pi, which keys auth by provider name.
//!
//! Stored at `~/.atlas/credentials.json` (0600), alongside — but separate from —
//! the xAI-specific `auth.json`, so the intricate `AuthManager`/OIDC machinery
//! is left untouched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::agent::oauth_providers::{OAuthTokens, SubscriptionProvider, refresh_tokens};

/// One stored credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    /// A saved API key (referenced by `credential = { named = "<id>" }`).
    ApiKey { key: String },
    /// A subscription OAuth credential (referenced by
    /// `credential = { oauth = "<id>" }`), minted and refreshed via
    /// [`crate::agent::oauth_providers`].
    Oauth {
        provider: SubscriptionProvider,
        tokens: OAuthTokens,
    },
}

impl Credential {
    /// The bearer to send right now, without refreshing. For OAuth this is the
    /// stored access token even if near expiry (callers should
    /// [`CredentialStore::ensure_fresh`] first; a stale token 401s and triggers
    /// the sampler's normal auth-retry path).
    pub fn current_bearer(&self) -> Option<String> {
        match self {
            Self::ApiKey { key } => Some(key.clone()),
            Self::Oauth { tokens, .. } => Some(tokens.access.clone()),
        }
    }
}

/// The on-disk credential store: `id -> Credential`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialStore {
    entries: BTreeMap<String, Credential>,
}

impl CredentialStore {
    /// Default path: `~/.atlas/credentials.json`.
    pub fn default_path() -> PathBuf {
        xai_grok_config::grok_home().join("credentials.json")
    }

    /// Load the store from `path`, returning an empty store if it does not exist.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Persist the store to `path` with `0600` permissions (user-only).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Credential> {
        self.entries.get(id)
    }

    pub fn put(&mut self, id: impl Into<String>, credential: Credential) {
        self.entries.insert(id.into(), credential);
    }

    pub fn remove(&mut self, id: &str) -> Option<Credential> {
        self.entries.remove(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// The bearer for `id` right now (no refresh). `None` if unknown.
    pub fn current_bearer(&self, id: &str) -> Option<String> {
        self.get(id).and_then(Credential::current_bearer)
    }
}

/// Refresh the OAuth credential `id` if its access token is expired, persisting
/// the rotated tokens. No-op for API-key credentials or fresh tokens. Returns
/// `true` if a refresh actually happened.
pub async fn ensure_fresh(
    store: &Arc<RwLock<CredentialStore>>,
    path: &Path,
    id: &str,
    client: &reqwest::Client,
) -> anyhow::Result<bool> {
    let refreshable = {
        let guard = store.read().expect("credential store lock poisoned");
        match guard.get(id) {
            Some(Credential::Oauth { provider, tokens }) if tokens.is_expired() => {
                tokens.refresh.clone().map(|rt| (*provider, rt))
            }
            _ => None,
        }
    };
    let Some((provider, refresh_token)) = refreshable else {
        return Ok(false);
    };
    let fresh = refresh_tokens(provider, &refresh_token, client).await?;
    {
        let mut guard = store.write().expect("credential store lock poisoned");
        guard.put(
            id,
            Credential::Oauth {
                provider,
                tokens: fresh,
            },
        );
        guard.save(path)?;
    }
    Ok(true)
}

/// Refresh every expired OAuth credential in the store. One provider failure
/// does not prevent the remaining credentials from being refreshed.
pub async fn ensure_all_fresh(path: &Path, client: &reqwest::Client) -> anyhow::Result<usize> {
    let store = Arc::new(RwLock::new(CredentialStore::load(path)?));
    let ids = {
        let guard = store
            .read()
            .map_err(|_| anyhow::anyhow!("credential store lock poisoned"))?;
        guard
            .entries
            .iter()
            .filter_map(|(id, credential)| {
                matches!(credential, Credential::Oauth { .. }).then_some(id.clone())
            })
            .collect::<Vec<_>>()
    };
    let mut refreshed = 0;
    let mut failures = Vec::new();
    for id in ids {
        match ensure_fresh(&store, path, &id, client).await {
            Ok(true) => refreshed += 1,
            Ok(false) => {}
            Err(error) => failures.push(format!("{id}: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(refreshed)
    } else {
        anyhow::bail!(
            "failed to refresh OAuth credentials: {}",
            failures.join("; ")
        )
    }
}

pub async fn ensure_all_fresh_default() -> anyhow::Result<usize> {
    let path = CredentialStore::default_path();
    let client = reqwest::Client::new();
    ensure_all_fresh(&path, &client).await
}

/// Run a subscription provider's OAuth login and persist the resulting tokens
/// to the credential store under the provider's id. Returns the credential id
/// that a `[connection.*]` should reference via `credential = { oauth = "<id>" }`.
///
/// Only the PKCE-loopback providers (Claude Pro/Max, ChatGPT Codex) are wired
/// here; GitHub Copilot's device-code flow is a follow-up. This is the single
/// call a `grok login <provider>` command needs; it lives in the shell crate so
/// the CLI hook stays a one-liner.
///
/// Live login requires the user's subscription and a browser — unverified in CI.
pub async fn login_and_store(
    provider: SubscriptionProvider,
    path: &Path,
    client: &reqwest::Client,
    open_browser: bool,
) -> anyhow::Result<String> {
    let tokens =
        crate::agent::oauth_providers::run_loopback_login(provider, client, open_browser).await?;
    let mut store = CredentialStore::load(path)?;
    let id = provider.id().to_owned();
    store.put(id.clone(), Credential::Oauth { provider, tokens });
    store.save(path)?;
    Ok(id)
}

/// A [`xai_grok_sampler::BearerResolver`] backed by the credential store, so a
/// connection's OAuth/named credential feeds the sampler's live per-request
/// bearer the same way xAI session tokens do. Refresh is driven out-of-band via
/// [`ensure_fresh`]; this resolver only reads the current token.
#[derive(Clone)]
pub struct StoreBearerResolver {
    store: Arc<RwLock<CredentialStore>>,
    credential_id: String,
}

impl StoreBearerResolver {
    pub fn new(store: Arc<RwLock<CredentialStore>>, credential_id: impl Into<String>) -> Self {
        Self {
            store,
            credential_id: credential_id.into(),
        }
    }
}

impl std::fmt::Debug for StoreBearerResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreBearerResolver")
            .field("credential_id", &self.credential_id)
            .finish()
    }
}

impl xai_grok_sampler::BearerResolver for StoreBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        self.store.read().ok()?.current_bearer(&self.credential_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_api_key_and_oauth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");

        let mut store = CredentialStore::default();
        store.put(
            "openrouter-work",
            Credential::ApiKey {
                key: "sk-or-123".into(),
            },
        );
        store.put(
            "anthropic",
            Credential::Oauth {
                provider: SubscriptionProvider::Anthropic,
                tokens: OAuthTokens {
                    access: "acc".into(),
                    refresh: Some("ref".into()),
                    expires_at_ms: 9_999_999_999_999,
                },
            },
        );
        store.save(&path).unwrap();

        let loaded = CredentialStore::load(&path).unwrap();
        assert_eq!(
            loaded.current_bearer("openrouter-work"),
            Some("sk-or-123".into())
        );
        assert_eq!(loaded.current_bearer("anthropic"), Some("acc".into()));
        assert_eq!(loaded.current_bearer("missing"), None);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::load(&dir.path().join("nope.json")).unwrap();
        assert!(store.ids().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        CredentialStore::default().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn bearer_resolver_reads_store() {
        use xai_grok_sampler::BearerResolver;
        let mut store = CredentialStore::default();
        store.put("c1", Credential::ApiKey { key: "k".into() });
        let resolver = StoreBearerResolver::new(Arc::new(RwLock::new(store)), "c1");
        assert_eq!(resolver.current_bearer(), Some("k".into()));
    }
}
