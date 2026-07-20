//! Provider-independent **connection** and **credential** model.
//!
//! A *connection* is a reusable bundle of `(adapter, endpoint, credential)` that
//! many models can share. A *credential* describes how to authenticate and is
//! referenced independently of any connection — so a single adapter (e.g. the
//! xAI Responses wire protocol) can back many independent accounts (personal
//! subscription, work key, project key) without special-casing anything.
//!
//! This implements the "**a provider is not an account**" split:
//!
//! | axis        | what it is                                    | represented by |
//! |-------------|-----------------------------------------------|----------------|
//! | adapter     | the wire protocol / how to talk to an endpoint| [`ApiBackend`] |
//! | connection  | endpoint + adapter + a credential reference   | [`ConnectionConfig`] |
//! | credential  | how to authenticate, referenced by id         | [`CredentialRef`] |
//! | model       | references a connection by id                 | `ModelEntryConfig.connection` |
//!
//! Inspired by the Pi Agent Harness (`earendil-works/pi`), whose `api` field is
//! the same adapter axis and whose named "provider" is really a connection.
//! Unlike Pi — which keys auth by provider name, so a provider effectively *is*
//! an account — here a credential is first-class and referenced by id, which is
//! what makes multiple independent connections for the same provider possible.
//!
//! ## Backward compatibility
//!
//! Existing `[model.*]` entries carry endpoint/auth inline (`base_url`,
//! `api_key`/`env_key`, `api_backend`, …). Those are preserved: a model with no
//! `connection` behaves exactly as before (an implicit anonymous connection).
//! A model that sets `connection = "<id>"` inherits any field it does not set
//! itself from the named `[connection.<id>]` — model-level fields always win.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use xai_grok_sampler::AuthScheme;
use xai_grok_sampling_types::ApiBackend;

use crate::agent::config::{EnvKeys, ModelEntry};

/// A built-in API-key provider that can reuse one of Atlas's existing wire
/// adapters. The registry is intentionally data-only: adding another
/// OpenAI-compatible or Anthropic-compatible provider should not require
/// changing the login UI, model resolver, or request client.
#[derive(Clone, Debug)]
pub struct ApiKeyProviderPreset {
    pub id: &'static str,
    pub display_name: &'static str,
    pub env_key: &'static str,
    pub default_model: &'static str,
    pub connection: ConnectionConfig,
}

/// How a connection authenticates. Externally tagged so TOML reads naturally:
///
/// ```toml
/// credential = "xai"                       # built-in xAI resolution
/// credential = { api_key = "$OPENAI_API_KEY" }
/// credential = { env = "ANTHROPIC_API_KEY" }
/// credential = { env = ["LC_TOKEN", "TOKEN"] }
/// credential = { oauth = "anthropic" }     # stored subscription (via /login)
/// credential = { named = "my-saved-key" }  # stored api key (via /login)
/// credential = "none"                      # no credential material
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRef {
    /// xAI built-in resolution: per-model key → session token → `XAI_API_KEY`
    /// → `env_key`. Preserves the single-vendor behavior for the default
    /// connection.
    Xai,
    /// A literal key or a [config value](resolve_config_value)
    /// (`$ENV`, `${ENV}`, `!command`, or a literal).
    ApiKey(String),
    /// Environment variable name(s); first set, non-empty value wins.
    Env(EnvKeys),
    /// An OAuth subscription credential stored under this provider id in the
    /// credential store (populated by `/login`). Resolved to a live, refreshing
    /// bearer at request time rather than a static key.
    Oauth(String),
    /// A named credential (typically a saved API key) stored under this id in
    /// the credential store.
    Named(String),
    /// No credential material (e.g. a keyless local server).
    None,
}

impl Default for CredentialRef {
    fn default() -> Self {
        Self::Xai
    }
}

impl CredentialRef {
    /// The stored-credential id this reference points at, if any
    /// (`oauth`/`named`). Used by the credential store to look up a bearer.
    pub fn stored_id(&self) -> Option<&str> {
        match self {
            Self::Oauth(id) | Self::Named(id) => Some(id.as_str()),
            _ => None,
        }
    }

    /// `true` when this reference is an OAuth subscription (needs a live,
    /// refreshing bearer resolver rather than a static key).
    pub fn is_oauth(&self) -> bool {
        matches!(self, Self::Oauth(_))
    }
}

/// A named, reusable connection parsed from `[connection.<id>]`.
///
/// Every field except `credential` is an **overlay**: it is applied to a model
/// only when the model does not set that field itself. This lets one connection
/// back many models while any model can still override a single field.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// Wire-protocol adapter for this connection (`chat_completions`,
    /// `responses`, `messages`). Applied to models that don't set `api_backend`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<ApiBackend>,
    /// Endpoint base URL, e.g. `https://api.anthropic.com/v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Endpoint used specifically for API-key (non-session) auth, mirroring the
    /// per-model `api_base_url` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    /// Authorization scheme (`bearer` or `x_api_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_scheme: Option<AuthScheme>,
    /// Extra request headers merged into every model on this connection
    /// (model-level headers win on key conflicts).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub extra_headers: IndexMap<String, String>,
    /// How this connection authenticates.
    #[serde(default)]
    pub credential: CredentialRef,
}

impl ConnectionConfig {
    /// Seed a model entry with this connection's endpoint/adapter/auth as the
    /// **base**, before the model's own `[model.*]` overrides are applied on
    /// top. This gives the intended precedence — connection provides defaults,
    /// the model always wins on any field it sets itself
    /// (`ConfigModelOverride::apply` runs after this and only overwrites fields
    /// the model specified).
    ///
    /// The credential is translated into the model's existing inline auth fields
    /// where possible (`api_key`/`env_key`), so all downstream resolution
    /// (`resolve_credentials`, `sampling_config_for_model`) works unchanged.
    /// `Xai` leaves the built-in xAI resolution in place. `Oauth`/`Named` refer
    /// to stored credentials resolved later by the credential store (they set no
    /// static key here).
    pub fn apply_as_base(&self, entry: &mut ModelEntry) {
        let path = crate::agent::credential_store::CredentialStore::default_path();
        let store =
            crate::agent::credential_store::CredentialStore::load(&path).unwrap_or_default();
        self.apply_as_base_with_store(entry, &store);
    }

    pub(crate) fn apply_as_base_with_store(
        &self,
        entry: &mut ModelEntry,
        store: &crate::agent::credential_store::CredentialStore,
    ) {
        if let Some(base_url) = &self.base_url {
            entry.info.base_url = base_url.clone();
        }
        if let Some(adapter) = &self.adapter {
            entry.info.api_backend = adapter.clone();
        }
        if let Some(scheme) = self.auth_scheme {
            entry.info.auth_scheme = scheme;
        }
        if self.api_base_url.is_some() {
            entry.api_base_url = self.api_base_url.clone();
        }
        if !self.extra_headers.is_empty() {
            entry.info.extra_headers = self.extra_headers.clone();
        }

        match &self.credential {
            // Preserve today's single-vendor resolution / no-op cases.
            CredentialRef::Xai | CredentialRef::None => {}
            CredentialRef::ApiKey(value) => {
                entry.api_key = resolve_config_value(value);
            }
            CredentialRef::Env(keys) => {
                entry.env_key = Some(keys.clone());
            }
            // Stored credentials (saved API keys / OAuth subscriptions): inject
            // the current bearer from the credential store as the model's key.
            // This is a snapshot; a live-refreshing `bearer_resolver` (analogous
            // to the xAI session-token path) is the follow-up seam. A missing
            // credential leaves the key unset — the model is simply unavailable,
            // exactly like an unset `env_key`.
            CredentialRef::Named(id) | CredentialRef::Oauth(id) => {
                entry.api_key = store.current_bearer(id);
                if let Some(crate::agent::credential_store::Credential::Oauth {
                    provider: crate::agent::oauth_providers::SubscriptionProvider::OpenAiCodex,
                    tokens,
                }) = store.get(id)
                    && let Some(account_id) =
                        crate::agent::oauth_providers::openai_chatgpt_account_id(&tokens.access)
                {
                    entry
                        .info
                        .extra_headers
                        .insert("ChatGPT-Account-ID".to_owned(), account_id);
                }
            }
        }
    }
}

/// Built-in connections shipped with the CLI so common providers can be used
/// without writing a `[connection.*]` block. A user-defined `[connection.<id>]`
/// with the same id fully overrides the built-in (Pi-style provider override).
///
/// These are **API-key** connections keyed off the conventional environment
/// variable for each provider (matching Pi's `env-api-keys.ts` names). Models
/// reference them by id, e.g. `[model.gpt-5.1] connection = "openai"`.
pub fn builtin_connections() -> IndexMap<String, ConnectionConfig> {
    let mut m: IndexMap<String, ConnectionConfig> = api_key_provider_presets()
        .into_iter()
        .map(|preset| (preset.id.to_owned(), preset.connection))
        .collect();
    m.insert(
        "openai-codex".to_owned(),
        ConnectionConfig {
            adapter: Some(ApiBackend::Responses),
            base_url: Some("https://chatgpt.com/backend-api/codex".to_owned()),
            extra_headers: [
                // ChatGPT's Codex endpoint uses this to select the Codex
                // client behavior, including its tool-enabled agent flow.
                ("originator".to_owned(), "codex_cli_rs".to_owned()),
                (
                    "OpenAI-Beta".to_owned(),
                    "responses=experimental".to_owned(),
                ),
            ]
            .into_iter()
            .collect(),
            credential: CredentialRef::Oauth("openai-codex".to_owned()),
            ..Default::default()
        },
    );
    // NOTE: the former `anthropic-subscription` connection (Claude Pro/Max via a
    // reverse-engineered OAuth flow hitting the raw Messages API) has been
    // removed. Subscription use now goes through the Claude Agent SDK harness
    // (`claude-agent` below), where the agentic work is done by Anthropic's own
    // harness and auth is delegated to `claude login`. See
    // `crate::agent::claude_agent`.

    // Routing marker for the Claude Agent SDK harness backend. This is NOT an
    // HTTP connection — a model referencing `connection = "claude-agent"` is
    // executed by the harness subprocess (see [`claude_agent::should_use_claude_harness`]),
    // so the endpoint/adapter fields are intentionally unset. The entry exists
    // only so the id resolves in the model picker and `/login`.
    m.insert(
        crate::agent::claude_agent::CONNECTION_ID.to_owned(),
        ConnectionConfig {
            // Sentinel endpoint — never dialed; the turn loop detects it and
            // routes to the harness subprocess. See `claude_agent::HARNESS_BASE_URL`.
            base_url: Some(crate::agent::claude_agent::HARNESS_BASE_URL.to_owned()),
            credential: CredentialRef::None,
            ..Default::default()
        },
    );
    m
}

/// API-key providers ported from Pi's provider registry that are compatible
/// with Atlas's current adapters. Providers that need a distinct transport
/// (Bedrock Converse, native Gemini/Vertex, Azure Responses) are deliberately
/// excluded instead of being presented as working connections.
pub fn api_key_provider_presets() -> Vec<ApiKeyProviderPreset> {
    fn openai_compatible(
        id: &'static str,
        display_name: &'static str,
        base_url: &'static str,
        env_key: &'static str,
        default_model: &'static str,
    ) -> ApiKeyProviderPreset {
        ApiKeyProviderPreset {
            id,
            display_name,
            env_key,
            default_model,
            connection: ConnectionConfig {
                adapter: Some(ApiBackend::ChatCompletions),
                base_url: Some(base_url.to_owned()),
                credential: CredentialRef::Env(EnvKeys::single(env_key)),
                ..Default::default()
            },
        }
    }

    fn anthropic_compatible(
        id: &'static str,
        display_name: &'static str,
        base_url: &'static str,
        env_key: &'static str,
        default_model: &'static str,
    ) -> ApiKeyProviderPreset {
        ApiKeyProviderPreset {
            id,
            display_name,
            env_key,
            default_model,
            connection: ConnectionConfig {
                adapter: Some(ApiBackend::Messages),
                base_url: Some(base_url.to_owned()),
                auth_scheme: Some(AuthScheme::XApiKey),
                extra_headers: [("anthropic-version".to_owned(), "2023-06-01".to_owned())]
                    .into_iter()
                    .collect(),
                credential: CredentialRef::Env(EnvKeys::single(env_key)),
                ..Default::default()
            },
        }
    }

    let mut providers = vec![
        ApiKeyProviderPreset {
            id: "openai",
            display_name: "OpenAI",
            env_key: "OPENAI_API_KEY",
            default_model: "gpt-5.5",
            connection: ConnectionConfig {
                adapter: Some(ApiBackend::Responses),
                base_url: Some("https://api.openai.com/v1".to_owned()),
                credential: CredentialRef::Env(EnvKeys::single("OPENAI_API_KEY")),
                ..Default::default()
            },
        },
        anthropic_compatible(
            "anthropic",
            "Anthropic",
            "https://api.anthropic.com/v1",
            "ANTHROPIC_API_KEY",
            "claude-opus-4-8",
        ),
        openai_compatible(
            "openrouter",
            "OpenRouter",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            "moonshotai/kimi-k2.6",
        ),
        // LiteLLM's proxy implements the OpenAI-compatible surface. The
        // endpoint is intentionally the local default; `/login litellm`
        // prompts for a distinct connection name and lets users override it
        // for a remote proxy.
        openai_compatible(
            "litellm",
            "LiteLLM Proxy",
            "http://localhost:4000/v1",
            "LITELLM_API_KEY",
            "gpt-4o",
        ),
        openai_compatible(
            "deepseek",
            "DeepSeek",
            "https://api.deepseek.com",
            "DEEPSEEK_API_KEY",
            "deepseek-v4-pro",
        ),
        openai_compatible(
            "google",
            "Google Gemini",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "GEMINI_API_KEY",
            "gemini-3.1-pro-preview",
        ),
        openai_compatible(
            "groq",
            "Groq",
            "https://api.groq.com/openai/v1",
            "GROQ_API_KEY",
            "openai/gpt-oss-120b",
        ),
        openai_compatible(
            "cerebras",
            "Cerebras",
            "https://api.cerebras.ai/v1",
            "CEREBRAS_API_KEY",
            "zai-glm-4.7",
        ),
        openai_compatible(
            "nvidia",
            "NVIDIA NIM",
            "https://integrate.api.nvidia.com/v1",
            "NVIDIA_API_KEY",
            "nvidia/nemotron-3-super-120b-a12b",
        ),
        openai_compatible(
            "zai",
            "Z.AI",
            "https://api.z.ai/api/coding/paas/v4",
            "ZAI_API_KEY",
            "glm-5.1",
        ),
        openai_compatible(
            "zai-coding-cn",
            "Z.AI Coding CN",
            "https://open.bigmodel.cn/api/coding/paas/v4",
            "ZAI_CODING_CN_API_KEY",
            "glm-5.1",
        ),
        openai_compatible(
            "mistral",
            "Mistral",
            "https://api.mistral.ai/v1",
            "MISTRAL_API_KEY",
            "devstral-medium-latest",
        ),
        anthropic_compatible(
            "minimax",
            "MiniMax",
            "https://api.minimax.io/anthropic/v1",
            "MINIMAX_API_KEY",
            "MiniMax-M2.7",
        ),
        anthropic_compatible(
            "minimax-cn",
            "MiniMax CN",
            "https://api.minimaxi.com/anthropic/v1",
            "MINIMAX_CN_API_KEY",
            "MiniMax-M2.7",
        ),
        openai_compatible(
            "moonshotai",
            "Moonshot AI",
            "https://api.moonshot.ai/v1",
            "MOONSHOT_API_KEY",
            "kimi-k2.6",
        ),
        openai_compatible(
            "moonshotai-cn",
            "Moonshot AI CN",
            "https://api.moonshot.cn/v1",
            "MOONSHOT_API_KEY",
            "kimi-k2.6",
        ),
        openai_compatible(
            "huggingface",
            "Hugging Face",
            "https://router.huggingface.co/v1",
            "HF_TOKEN",
            "moonshotai/Kimi-K2.6",
        ),
        ApiKeyProviderPreset {
            id: "fireworks",
            display_name: "Fireworks",
            env_key: "FIREWORKS_API_KEY",
            default_model: "accounts/fireworks/models/kimi-k2p6",
            connection: ConnectionConfig {
                adapter: Some(ApiBackend::Messages),
                base_url: Some("https://api.fireworks.ai/inference/v1".to_owned()),
                auth_scheme: Some(AuthScheme::Bearer),
                credential: CredentialRef::Env(EnvKeys::single("FIREWORKS_API_KEY")),
                ..Default::default()
            },
        },
        openai_compatible(
            "together",
            "Together AI",
            "https://api.together.ai/v1",
            "TOGETHER_API_KEY",
            "moonshotai/Kimi-K2.6",
        ),
        anthropic_compatible(
            "kimi-coding",
            "Kimi For Coding",
            "https://api.kimi.com/coding/v1",
            "KIMI_API_KEY",
            "kimi-for-coding",
        ),
        anthropic_compatible(
            "vercel-ai-gateway",
            "Vercel AI Gateway",
            "https://ai-gateway.vercel.sh/v1",
            "AI_GATEWAY_API_KEY",
            "zai/glm-5.1",
        ),
        openai_compatible(
            "xiaomi",
            "Xiaomi MiMo",
            "https://api.xiaomimimo.com/v1",
            "XIAOMI_API_KEY",
            "mimo-v2.5-pro",
        ),
        openai_compatible(
            "xiaomi-token-plan-cn",
            "Xiaomi MiMo Token Plan (China)",
            "https://token-plan-cn.xiaomimimo.com/v1",
            "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            "mimo-v2.5-pro",
        ),
        openai_compatible(
            "xiaomi-token-plan-ams",
            "Xiaomi MiMo Token Plan (Amsterdam)",
            "https://token-plan-ams.xiaomimimo.com/v1",
            "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            "mimo-v2.5-pro",
        ),
        openai_compatible(
            "xiaomi-token-plan-sgp",
            "Xiaomi MiMo Token Plan (Singapore)",
            "https://token-plan-sgp.xiaomimimo.com/v1",
            "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            "mimo-v2.5-pro",
        ),
        openai_compatible(
            "ant-ling",
            "Ant Ling",
            "https://api.ant-ling.com/v1",
            "ANT_LING_API_KEY",
            "Ring-2.6-1T",
        ),
    ];
    providers.sort_by(|a, b| a.display_name.cmp(b.display_name));
    providers
}

/// Resolve a connection id against user-defined connections first, then the
/// built-ins. `None` when neither defines it.
pub fn resolve_connection<'a>(
    user_connections: &'a IndexMap<String, ConnectionConfig>,
    id: &str,
    builtins: &'a IndexMap<String, ConnectionConfig>,
) -> Option<&'a ConnectionConfig> {
    user_connections.get(id).or_else(|| builtins.get(id))
}

/// Resolve a Pi-style config value: `$ENV`/`${ENV}` interpolation, a leading
/// `!command` (whose stdout is used), `$$`/`$!` escapes, or a literal. Returns
/// `None` when a referenced environment variable is unset or a command fails —
/// callers treat `None` as "no credential".
pub fn resolve_config_value(raw: &str) -> Option<String> {
    resolve_config_value_with(raw, |name| std::env::var(name).ok(), run_command)
}

/// Testable core of [`resolve_config_value`] with injectable env + command.
pub fn resolve_config_value_with(
    raw: &str,
    mut getenv: impl FnMut(&str) -> Option<String>,
    mut run: impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    // Escapes: a value that is exactly "$$…" / "$!…" emits a literal $ / !.
    if let Some(rest) = raw.strip_prefix("$$") {
        return Some(format!("${rest}"));
    }
    if let Some(rest) = raw.strip_prefix("$!") {
        return Some(format!("!{rest}"));
    }
    // Shell command: the whole value is a command, stdout (trimmed) is the key.
    if let Some(cmd) = raw.strip_prefix('!') {
        return run(cmd);
    }
    // Environment interpolation anywhere in the string.
    if raw.contains('$') {
        return interpolate_env(raw, &mut getenv);
    }
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Replace `$NAME` and `${NAME}` with environment values. If any referenced
/// variable is unset the whole value is treated as unresolved (`None`), matching
/// Pi's behavior for missing secrets.
fn interpolate_env(raw: &str, getenv: &mut impl FnMut(&str) -> Option<String>) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let name = if chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut name = String::new();
            for nc in chars.by_ref() {
                if nc == '}' {
                    break;
                }
                name.push(nc);
            }
            name
        } else {
            let mut name = String::new();
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_alphanumeric() || nc == '_' {
                    name.push(nc);
                    chars.next();
                } else {
                    break;
                }
            }
            name
        };
        if name.is_empty() {
            out.push('$');
            continue;
        }
        out.push_str(&getenv(&name)?);
    }
    let trimmed = out.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Run a `!command` value through the shell and return trimmed stdout on success.
fn run_command(cmd: &str) -> Option<String> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn literal_value_passes_through() {
        assert_eq!(
            resolve_config_value_with("sk-ant-123", |_| None, |_| None),
            Some("sk-ant-123".to_owned())
        );
    }

    #[test]
    fn env_interpolation_simple_and_braced() {
        let e = env(&[("OPENAI_API_KEY", "sk-openai"), ("SUFFIX", "xyz")]);
        assert_eq!(
            resolve_config_value_with("$OPENAI_API_KEY", &e, |_| None),
            Some("sk-openai".to_owned())
        );
        assert_eq!(
            resolve_config_value_with("pre-${SUFFIX}-post", &e, |_| None),
            Some("pre-xyz-post".to_owned())
        );
    }

    #[test]
    fn missing_env_is_unresolved() {
        assert_eq!(
            resolve_config_value_with("$NOT_SET", |_| None, |_| None),
            None
        );
    }

    #[test]
    fn dollar_and_bang_escapes() {
        assert_eq!(
            resolve_config_value_with("$$literal", |_| None, |_| None),
            Some("$literal".to_owned())
        );
        assert_eq!(
            resolve_config_value_with("$!literal", |_| None, |_| None),
            Some("!literal".to_owned())
        );
    }

    #[test]
    fn command_value_uses_stdout() {
        assert_eq!(
            resolve_config_value_with(
                "!echo hunter2",
                |_| None,
                |cmd| {
                    assert_eq!(cmd, "echo hunter2");
                    Some("hunter2".to_owned())
                }
            ),
            Some("hunter2".to_owned())
        );
    }

    #[test]
    fn credential_ref_toml_shapes() {
        // Unit variant reads as a bare string.
        let xai: CredentialRef = toml::from_str("v = \"xai\"").map(|w: Wrap| w.v).unwrap();
        assert_eq!(xai, CredentialRef::Xai);

        let api: CredentialRef = toml::from_str("v = { api_key = \"$OPENAI_API_KEY\" }")
            .map(|w: Wrap| w.v)
            .unwrap();
        assert_eq!(api, CredentialRef::ApiKey("$OPENAI_API_KEY".to_owned()));

        let oauth: CredentialRef = toml::from_str("v = { oauth = \"anthropic\" }")
            .map(|w: Wrap| w.v)
            .unwrap();
        assert_eq!(oauth, CredentialRef::Oauth("anthropic".to_owned()));
        assert!(oauth.is_oauth());
        assert_eq!(oauth.stored_id(), Some("anthropic"));
    }

    #[test]
    fn chatgpt_codex_connection_uses_the_codex_originator() {
        let connections = builtin_connections();
        let codex = connections.get("openai-codex").unwrap();
        assert_eq!(
            codex.extra_headers.get("originator").map(String::as_str),
            Some("codex_cli_rs")
        );
    }

    #[test]
    fn api_key_presets_are_unique_and_routable() {
        let presets = api_key_provider_presets();
        assert_eq!(presets.len(), 26);
        let mut ids = std::collections::HashSet::new();
        for preset in presets {
            assert!(ids.insert(preset.id), "duplicate provider id {}", preset.id);
            assert!(!preset.env_key.is_empty());
            assert!(!preset.default_model.is_empty());
            assert!(preset.connection.adapter.is_some());
            assert!(preset.connection.base_url.is_some());
            assert!(matches!(
                preset.connection.credential,
                CredentialRef::Env(_)
            ));
        }
        let litellm = api_key_provider_presets()
            .into_iter()
            .find(|preset| preset.id == "litellm")
            .expect("LiteLLM preset should be registered");
        assert_eq!(litellm.env_key, "LITELLM_API_KEY");
        assert_eq!(
            litellm.connection.base_url.as_deref(),
            Some("http://localhost:4000/v1")
        );
        let fireworks = api_key_provider_presets()
            .into_iter()
            .find(|preset| preset.id == "fireworks")
            .expect("Fireworks preset should be registered");
        assert_eq!(fireworks.connection.auth_scheme, Some(AuthScheme::Bearer));
    }

    #[derive(Deserialize)]
    struct Wrap {
        v: CredentialRef,
    }
}
