//! Model discovery for connected third-party providers.
//!
//! Provider catalogs are optional acceleration, not a startup dependency:
//! every successful non-empty response replaces the on-disk cache, while a
//! failed or empty response leaves the last-known-good cache untouched.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

use crate::agent::config::{Config, ModelEntry};
use crate::agent::credential_store::{Credential, CredentialStore};
use crate::agent::oauth_providers::SubscriptionProvider;

const OPENAI_CODEX_CONNECTION_ID: &str = "openai-codex";
const OPENAI_CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
/// Highest Codex catalog protocol version whose returned model metadata Atlas
/// understands. This is also stored with the cache: a newer protocol can make
/// additional account-entitled models visible, so an older cached response must
/// not keep the picker artificially truncated.
const OPENAI_CODEX_COMPAT_VERSION: &str = "0.144.5";
const DEFAULT_CACHE_MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ProviderCatalog {
    /// Compatibility version used to fetch this catalog. Older cache files did
    /// not carry it and are intentionally refreshed once.
    #[serde(default)]
    catalog_version: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    models: Vec<ProviderCatalogModel>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ProviderCatalogModel {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default = "default_true")]
    supported_in_api: bool,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<CodexReasoningLevel>,
    /// The server-advertised request tiers, e.g. `priority` for Fast mode.
    /// Keep this even before the picker grows its speed control so refreshing
    /// the catalog never throws away capability information.
    #[serde(default)]
    service_tiers: Vec<CodexServiceTier>,
    #[serde(default)]
    default_service_tier: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CodexReasoningLevel {
    effort: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CodexServiceTier {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

fn default_true() -> bool {
    true
}

fn cache_path() -> PathBuf {
    xai_grok_config::grok_home()
        .join("provider_models")
        .join("openai-codex.json")
}

fn cache_is_fresh(path: &Path, max_age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age <= max_age)
}

fn load_cached_catalog() -> Option<ProviderCatalog> {
    let bytes = std::fs::read(cache_path()).ok()?;
    let catalog: ProviderCatalog = serde_json::from_slice(&bytes).ok()?;
    (!catalog.models.is_empty()).then_some(catalog)
}

fn save_catalog(path: &Path, catalog: &ProviderCatalog) -> anyhow::Result<()> {
    anyhow::ensure!(
        !catalog.models.is_empty(),
        "refusing to cache an empty model catalog"
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("provider catalog path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec_pretty(catalog)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(temporary, path)?;
    Ok(())
}

/// Refresh the connected ChatGPT account's model catalog when stale.
///
/// Errors are returned to the caller for logging, but never delete a prior
/// cache. The startup path therefore remains usable offline.
pub async fn refresh_openai_codex_catalog_if_stale(force: bool) -> anyhow::Result<usize> {
    let path = cache_path();
    let credential_path = CredentialStore::default_path();
    let initial_store = CredentialStore::load(&credential_path)?;
    let Some(initial_account_id) = openai_account_id(&initial_store) else {
        return Ok(0);
    };
    if !force
        && cache_is_fresh(&path, DEFAULT_CACHE_MAX_AGE)
        && let Some(catalog) = load_cached_catalog()
        && catalog.account_id.as_deref() == Some(initial_account_id.as_str())
        && catalog.catalog_version.as_deref() == Some(OPENAI_CODEX_COMPAT_VERSION)
    {
        return Ok(catalog.models.len());
    }

    let store = Arc::new(RwLock::new(initial_store));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    crate::agent::credential_store::ensure_fresh(
        &store,
        &credential_path,
        OPENAI_CODEX_CONNECTION_ID,
        &client,
    )
    .await?;

    let (access_token, account_id) = {
        let store = store
            .read()
            .map_err(|_| anyhow::anyhow!("credential store lock poisoned"))?;
        let Some(Credential::Oauth {
            provider: SubscriptionProvider::OpenAiCodex,
            tokens,
        }) = store.get(OPENAI_CODEX_CONNECTION_ID)
        else {
            return Ok(0);
        };
        let Some(account_id) =
            crate::agent::oauth_providers::openai_chatgpt_account_id(&tokens.access)
        else {
            anyhow::bail!("OpenAI credential is missing a ChatGPT account id");
        };
        (tokens.access.clone(), account_id)
    };

    let response = client
        .get(OPENAI_CODEX_MODELS_URL)
        .query(&[("client_version", OPENAI_CODEX_COMPAT_VERSION)])
        .bearer_auth(access_token)
        .header("ChatGPT-Account-ID", account_id.clone())
        .header("originator", "codex_cli_rs")
        .send()
        .await?;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        anyhow::bail!(
            "OpenAI Codex model discovery failed ({status}): {}",
            String::from_utf8_lossy(&body)
        );
    }
    let mut catalog: ProviderCatalog = serde_json::from_slice(&body)
        .map_err(|error| anyhow::anyhow!("invalid OpenAI Codex model catalog: {error}"))?;
    anyhow::ensure!(
        !catalog.models.is_empty(),
        "OpenAI Codex returned an empty model catalog"
    );
    catalog.account_id = Some(account_id);
    catalog.catalog_version = Some(OPENAI_CODEX_COMPAT_VERSION.to_owned());
    let count = catalog.models.len();
    save_catalog(&path, &catalog)?;
    Ok(count)
}

/// Refresh credentials and optional provider catalogs before any interactive
/// or headless session resolves its model list. Every step is fail-soft so
/// cached/bundled models remain usable during provider outages.
pub async fn prepare_connected_providers() {
    if let Err(error) = crate::agent::credential_store::ensure_all_fresh_default().await {
        tracing::warn!(%error, "failed to refresh one or more provider credentials");
    }
    if let Err(error) = refresh_openai_codex_catalog_if_stale(false).await {
        tracing::warn!(%error, "failed to refresh OpenAI Codex model catalog; using cache or fallback");
    }
}

fn openai_account_id(store: &CredentialStore) -> Option<String> {
    let Credential::Oauth {
        provider: SubscriptionProvider::OpenAiCodex,
        tokens,
    } = store.get(OPENAI_CODEX_CONNECTION_ID)?
    else {
        return None;
    };
    crate::agent::oauth_providers::openai_chatgpt_account_id(&tokens.access)
}

/// Add cached, account-entitled ChatGPT models to the resolved model list.
/// Returns the number of visible models added.
pub(crate) fn add_cached_openai_codex_models(
    cfg: &Config,
    resolved: &mut IndexMap<String, ModelEntry>,
    store: &CredentialStore,
) -> usize {
    let Some(catalog) = load_cached_catalog() else {
        return 0;
    };
    if catalog.account_id != openai_account_id(store) {
        return 0;
    }
    let builtins = crate::agent::connection::builtin_connections();
    let Some(connection) = crate::agent::connection::resolve_connection(
        &cfg.connections,
        OPENAI_CODEX_CONNECTION_ID,
        &builtins,
    ) else {
        return 0;
    };

    let mut added = 0;
    for model in catalog.models {
        if model.slug.trim().is_empty()
            || model.visibility.as_deref() == Some("hide")
            || !model.supported_in_api
        {
            continue;
        }
        let mut entry = ModelEntry::fallback(&model.slug, &cfg.endpoints);
        connection.apply_as_base_with_store(&mut entry, store);
        if !entry.has_own_credentials() {
            continue;
        }
        let reasoning_efforts = reasoning_options(&model);
        let reasoning_effort = model
            .default_reasoning_level
            .as_deref()
            .and_then(|effort| effort.parse().ok());
        entry.info.name = model.display_name;
        entry.info.description = model.description;
        if let Some(context_window) = model.context_window.and_then(NonZeroU64::new) {
            entry.info.context_window = context_window;
        }
        entry.info.reasoning_efforts = reasoning_efforts;
        entry.info.reasoning_effort = reasoning_effort;
        entry.info.supports_reasoning_effort = !entry.info.reasoning_efforts.is_empty();
        resolved.insert(
            format!("{OPENAI_CODEX_CONNECTION_ID}/{}", model.slug),
            entry,
        );
        added += 1;
    }
    added
}

fn reasoning_options(model: &ProviderCatalogModel) -> Vec<ReasoningEffortOption> {
    model
        .supported_reasoning_levels
        .iter()
        .filter_map(|level| {
            let value = level.effort.parse::<ReasoningEffort>().ok()?;
            Some(ReasoningEffortOption {
                id: level.effort.clone(),
                value,
                label: humanize(&level.effort),
                description: level.description.clone(),
                default: model.default_reasoning_level.as_deref() == Some(level.effort.as_str()),
            })
        })
        .collect()
}

fn humanize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_reasoning_is_forward_compatible_and_hidden_models_are_filtered() {
        let model = ProviderCatalogModel {
            slug: "gpt-test".to_owned(),
            display_name: None,
            description: None,
            context_window: Some(272_000),
            supported_in_api: true,
            visibility: Some("list".to_owned()),
            default_reasoning_level: Some("medium".to_owned()),
            supported_reasoning_levels: vec![
                CodexReasoningLevel {
                    effort: "medium".to_owned(),
                    description: None,
                },
                CodexReasoningLevel {
                    effort: "future-tier".to_owned(),
                    description: None,
                },
            ],
            service_tiers: vec![CodexServiceTier {
                id: "priority".to_owned(),
                name: Some("Fast".to_owned()),
                description: Some("Faster responses".to_owned()),
            }],
            default_service_tier: Some("priority".to_owned()),
        };
        let options = reasoning_options(&model);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].value, ReasoningEffort::Medium);
        assert!(options[0].default);
        assert_eq!(model.service_tiers[0].id, "priority");
        assert_eq!(model.service_tiers[0].name.as_deref(), Some("Fast"));
        assert_eq!(model.default_service_tier.as_deref(), Some("priority"));
    }
}
