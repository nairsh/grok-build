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
            // Stored credentials (saved API keys / OAuth subscriptions) are
            // resolved by the credential store at request time, not here.
            CredentialRef::Named(_) | CredentialRef::Oauth(_) => {}
        }
    }
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

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + '_ {
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
            resolve_config_value_with("!echo hunter2", |_| None, |cmd| {
                assert_eq!(cmd, "echo hunter2");
                Some("hunter2".to_owned())
            }),
            Some("hunter2".to_owned())
        );
    }

    #[test]
    fn credential_ref_toml_shapes() {
        // Unit variant reads as a bare string.
        let xai: CredentialRef = toml::from_str("v = \"xai\"")
            .map(|w: Wrap| w.v)
            .unwrap();
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

    #[derive(Deserialize)]
    struct Wrap {
        v: CredentialRef,
    }
}
