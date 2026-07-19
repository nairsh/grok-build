//! Built-in **subscription OAuth** providers: Claude Pro/Max, ChatGPT (Codex),
//! and GitHub Copilot.
//!
//! The flows, client IDs, endpoints, scopes, and PKCE parameters are ported
//! from the open-source Pi Agent Harness
//! (`@earendil-works/pi-ai/utils/oauth/{anthropic,openai-codex,github-copilot}`),
//! which are the officially/community-supported ways for a third-party CLI to
//! authenticate against these subscriptions. Ports rather than
//! reverse-engineering: the parameters are exact copies.
//!
//! This module is provider-only — it knows how to run each auth flow and mint /
//! refresh tokens. It does **not** know about connections or models; a
//! connection references a stored OAuth credential by id (see
//! [`crate::agent::connection::CredentialRef::Oauth`]), and the credential store
//! persists and refreshes the [`OAuthTokens`] this module produces.
//!
//! ## Verification status
//!
//! The PKCE derivation and authorize-URL construction are unit-tested offline.
//! The live token exchange/refresh and the `/login` UI wiring require the user's
//! own subscriptions and a browser, and are **not** verified in CI.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A subscription provider with a built-in OAuth flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionProvider {
    /// Anthropic Claude Pro/Max — authorization-code + PKCE, loopback callback.
    Anthropic,
    /// OpenAI ChatGPT Plus/Pro (Codex) — authorization-code + PKCE, loopback.
    OpenAiCodex,
    /// GitHub Copilot — device-code flow, then a Copilot token exchange.
    GithubCopilot,
}

impl SubscriptionProvider {
    /// The credential-store id / `credential = { oauth = "<id>" }` this provider
    /// authenticates.
    pub fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAiCodex => "openai-codex",
            Self::GithubCopilot => "github-copilot",
        }
    }

    /// Human-readable name shown in `/login`.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic (Claude Pro/Max)",
            Self::OpenAiCodex => "ChatGPT (Codex)",
            Self::GithubCopilot => "GitHub Copilot",
        }
    }

    /// Whether this provider uses the loopback authorization-code + PKCE flow
    /// ([`build_authorize_url`]) vs. the device-code flow ([`GithubCopilot`]).
    pub fn uses_pkce_loopback(self) -> bool {
        matches!(self, Self::Anthropic | Self::OpenAiCodex)
    }

    fn oauth(self) -> OAuthEndpoints {
        match self {
            // client id + endpoints copied verbatim from Pi's anthropic.ts.
            Self::Anthropic => OAuthEndpoints {
                client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                authorize_url: "https://claude.ai/oauth/authorize",
                token_url: "https://platform.claude.com/v1/oauth/token",
                redirect_uri: "http://localhost:53692/callback",
                scope: "org:create_api_key user:profile user:inference \
                        user:sessions:claude_code user:mcp_servers user:file_upload",
            },
            // from Pi's openai-codex.ts.
            Self::OpenAiCodex => OAuthEndpoints {
                client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
                authorize_url: "https://auth.openai.com/oauth/authorize",
                token_url: "https://auth.openai.com/oauth/token",
                redirect_uri: "http://localhost:1455/auth/callback",
                scope: "openid profile email offline_access",
            },
            // Copilot uses a device flow; the loopback fields are unused.
            Self::GithubCopilot => OAuthEndpoints {
                client_id: "Iv1.b507a08c87ecfe98",
                authorize_url: "https://github.com/login/device/code",
                token_url: "https://github.com/login/oauth/access_token",
                redirect_uri: "",
                scope: "read:user",
            },
        }
    }
}

struct OAuthEndpoints {
    client_id: &'static str,
    authorize_url: &'static str,
    token_url: &'static str,
    redirect_uri: &'static str,
    scope: &'static str,
}

/// OAuth tokens minted by a subscription flow. Persisted by the credential
/// store; `access` is the bearer used for inference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<String>,
    /// Unix-epoch milliseconds at which `access` should be considered expired
    /// (already includes a safety margin).
    pub expires_at_ms: u64,
}

impl OAuthTokens {
    /// Whether the access token is at/near expiry and should be refreshed.
    pub fn is_expired(&self) -> bool {
        now_ms() >= self.expires_at_ms
    }
}

/// Extract the ChatGPT account id carried by an OpenAI Codex access token.
///
/// ChatGPT subscription requests require this value in the
/// `ChatGPT-Account-ID` header. The token is already authenticated by TLS and
/// the provider; this parser only reads the JWT payload to route the request to
/// the account selected during login.
pub fn openai_chatgpt_account_id(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
}

/// PKCE material for the authorization-code flow.
#[derive(Clone, Debug)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a PKCE verifier + S256 challenge, matching Pi's derivation
/// (`base64url(sha256(base64url(random32)))`).
pub fn generate_pkce() -> Pkce {
    let random_bytes: [u8; 32] = rand::random();
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// Build the browser authorize URL for a PKCE-loopback provider. `state` is
/// carried through and validated on the callback; Pi reuses the verifier as the
/// state, which we mirror.
pub fn build_authorize_url(provider: SubscriptionProvider, pkce: &Pkce, state: &str) -> String {
    let e = provider.oauth();
    let params = [
        ("response_type", "code"),
        ("client_id", e.client_id),
        ("redirect_uri", e.redirect_uri),
        ("scope", e.scope),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
        .collect();
    // Anthropic additionally sets `code=true` to signal the manual-code path.
    let extra = if provider == SubscriptionProvider::Anthropic {
        "&code=true"
    } else {
        ""
    };
    format!("{}?{}{extra}", e.authorize_url, query.join("&"))
}

/// Response body shape shared by the token/refresh endpoints.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl TokenResponse {
    fn into_tokens(self) -> OAuthTokens {
        // 5-minute safety margin, matching Pi.
        let ttl_ms = self.expires_in.unwrap_or(3600).saturating_mul(1000);
        let expires_at_ms = now_ms()
            .saturating_add(ttl_ms)
            .saturating_sub(5 * 60 * 1000);
        OAuthTokens {
            access: self.access_token,
            refresh: self.refresh_token,
            expires_at_ms,
        }
    }
}

/// Exchange an authorization `code` for tokens (PKCE-loopback providers).
pub async fn exchange_authorization_code(
    provider: SubscriptionProvider,
    code: &str,
    state: &str,
    pkce: &Pkce,
    client: &reqwest::Client,
) -> anyhow::Result<OAuthTokens> {
    let e = provider.oauth();
    let body = authorization_code_body(provider, code, state, pkce, &e);
    let request = client
        .post(e.token_url)
        .header("Accept", "application/json");
    let request = if provider == SubscriptionProvider::OpenAiCodex {
        // Match the OAuth token endpoint's documented wire format. JSON happens
        // to be accepted in some deployments, but form encoding is the stable
        // contract used by the official Codex client.
        request.form(&body)
    } else {
        request.json(&body)
    };
    let resp = request.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!(
            "token exchange failed ({status}) at {}: {text}",
            e.token_url
        );
    }
    let parsed: TokenResponse = serde_json::from_str(&text)
        .map_err(|err| anyhow::anyhow!("invalid token response: {err}; body={text}"))?;
    Ok(parsed.into_tokens())
}

fn authorization_code_body(
    provider: SubscriptionProvider,
    code: &str,
    state: &str,
    pkce: &Pkce,
    endpoints: &OAuthEndpoints,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": endpoints.client_id,
        "code": code,
        "redirect_uri": endpoints.redirect_uri,
        "code_verifier": pkce.verifier,
    });

    // OpenAI validates `state` on the authorize callback but rejects it at the
    // token endpoint as an unknown parameter. Anthropic's exchange expects it.
    if provider == SubscriptionProvider::Anthropic {
        body["state"] = serde_json::Value::String(state.to_owned());
    }
    body
}

/// Refresh an expired access token using a stored refresh token
/// (PKCE-loopback providers).
pub async fn refresh_tokens(
    provider: SubscriptionProvider,
    refresh_token: &str,
    client: &reqwest::Client,
) -> anyhow::Result<OAuthTokens> {
    let e = provider.oauth();
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": e.client_id,
        "refresh_token": refresh_token,
    });
    let request = client
        .post(e.token_url)
        .header("Accept", "application/json");
    let request = if provider == SubscriptionProvider::OpenAiCodex {
        request.form(&body)
    } else {
        request.json(&body)
    };
    let resp = request.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("token refresh failed ({status}) at {}: {text}", e.token_url);
    }
    let mut parsed: TokenResponse = serde_json::from_str(&text)
        .map_err(|err| anyhow::anyhow!("invalid refresh response: {err}; body={text}"))?;
    // Providers may omit a rotated refresh token; keep the old one.
    if parsed.refresh_token.is_none() {
        parsed.refresh_token = Some(refresh_token.to_owned());
    }
    Ok(parsed.into_tokens())
}

/// Run the full browser-based authorization-code + PKCE login for a loopback
/// provider (Claude Pro/Max or ChatGPT Codex): start the local callback server,
/// open the browser to the authorize URL, wait for the redirect, and exchange
/// the code for tokens.
///
/// Verified only up to the browser handoff — completing the exchange needs the
/// user's live subscription. Copilot uses a device-code flow and is not handled
/// here.
pub async fn run_loopback_login(
    provider: SubscriptionProvider,
    client: &reqwest::Client,
    open_browser: bool,
) -> anyhow::Result<OAuthTokens> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    anyhow::ensure!(
        provider.uses_pkce_loopback(),
        "{} does not use the loopback login flow",
        provider.display_name()
    );
    let e = provider.oauth();
    let redirect = url::Url::parse(e.redirect_uri)?;
    let port = redirect.port().unwrap_or(80);
    let expected_path = redirect.path().to_owned();

    let pkce = generate_pkce();
    // Pi reuses the PKCE verifier as the OAuth state.
    let state = pkce.verifier.clone();

    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|err| anyhow::anyhow!("could not bind loopback callback port {port}: {err}"))?;

    let auth_url = build_authorize_url(provider, &pkce, &state);
    eprintln!(
        "Authorize {} in your browser:\n{auth_url}\n",
        provider.display_name()
    );
    if open_browser {
        let u = auth_url.clone();
        tokio::task::spawn_blocking(move || {
            let _ = webbrowser::open(&u);
        });
    }

    // Accept exactly one callback request and parse the query.
    let (mut socket, _) = listener.accept().await?;
    let mut buf = vec![0u8; 8192];
    let n = socket.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("malformed callback request"))?;

    let (code, got_state) = parse_callback_query(target, &expected_path)?;
    let body = if code.is_some() {
        "Authentication complete. You can close this window."
    } else {
        "Authentication failed. Check the terminal."
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;

    let code =
        code.ok_or_else(|| anyhow::anyhow!("callback did not include an authorization code"))?;
    anyhow::ensure!(
        got_state.as_deref() == Some(state.as_str()),
        "OAuth state mismatch"
    );
    exchange_authorization_code(provider, &code, &state, &pkce, client).await
}

/// Extract `code` and `state` from an HTTP request target
/// (`/callback?code=...&state=...`), verifying the path matches the redirect.
fn parse_callback_query(
    target: &str,
    expected_path: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    // A relative target has no base; graft a dummy origin so `url` can parse it.
    let parsed = url::Url::parse(&format!("http://localhost{target}"))
        .map_err(|err| anyhow::anyhow!("could not parse callback target {target:?}: {err}"))?;
    anyhow::ensure!(
        parsed.path() == expected_path,
        "unexpected callback path {:?} (want {expected_path:?})",
        parsed.path()
    );
    if let Some(err) = parsed.query_pairs().find(|(k, _)| k == "error") {
        anyhow::bail!("authorization failed: {}", err.1);
    }
    let mut code = None;
    let mut state = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }
    Ok((code, state))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let pkce = generate_pkce();
        // 32 random bytes → 43-char base64url verifier (matches Pi/oidc).
        assert_eq!(pkce.verifier.len(), 43);
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn authorize_url_has_pkce_and_client_id() {
        let pkce = Pkce {
            verifier: "v".into(),
            challenge: "chal".into(),
        };
        let url = build_authorize_url(SubscriptionProvider::Anthropic, &pkce, "state123");
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state123"));
        assert!(url.contains("&code=true"));

        let codex = build_authorize_url(SubscriptionProvider::OpenAiCodex, &pkce, "s");
        assert!(codex.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(!codex.contains("&code=true"));
    }

    #[test]
    fn provider_ids_are_stable() {
        assert_eq!(SubscriptionProvider::Anthropic.id(), "anthropic");
        assert_eq!(SubscriptionProvider::OpenAiCodex.id(), "openai-codex");
        assert_eq!(SubscriptionProvider::GithubCopilot.id(), "github-copilot");
        assert!(SubscriptionProvider::Anthropic.uses_pkce_loopback());
        assert!(!SubscriptionProvider::GithubCopilot.uses_pkce_loopback());
    }

    #[test]
    fn parses_callback_code_and_state() {
        let (code, state) =
            parse_callback_query("/callback?code=abc123&state=xyz", "/callback").unwrap();
        assert_eq!(code.as_deref(), Some("abc123"));
        assert_eq!(state.as_deref(), Some("xyz"));
    }

    #[test]
    fn openai_token_exchange_omits_state_but_anthropic_keeps_it() {
        let pkce = Pkce {
            verifier: "verifier".into(),
            challenge: "challenge".into(),
        };
        let openai = SubscriptionProvider::OpenAiCodex;
        let openai_body = authorization_code_body(openai, "code", "state", &pkce, &openai.oauth());
        assert_eq!(openai_body["code"], "code");
        assert_eq!(openai_body["code_verifier"], "verifier");
        assert!(openai_body.get("state").is_none());

        let anthropic = SubscriptionProvider::Anthropic;
        let anthropic_body =
            authorization_code_body(anthropic, "code", "state", &pkce, &anthropic.oauth());
        assert_eq!(anthropic_body["state"], "state");
    }

    #[test]
    fn rejects_wrong_callback_path_and_surfaces_errors() {
        assert!(parse_callback_query("/wrong?code=a", "/callback").is_err());
        assert!(parse_callback_query("/callback?error=access_denied", "/callback").is_err());
    }

    #[test]
    fn expiry_math_applies_safety_margin() {
        let t = TokenResponse {
            access_token: "a".into(),
            refresh_token: None,
            expires_in: Some(3600),
        }
        .into_tokens();
        // ~55 minutes out (3600s − 5min margin), not expired now.
        assert!(!t.is_expired());
    }

    #[test]
    fn extracts_openai_account_id_from_access_token() {
        let claims = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-123"
            }
        });
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        let token = format!("header.{payload}.signature");
        assert_eq!(
            openai_chatgpt_account_id(&token).as_deref(),
            Some("account-123")
        );
        assert_eq!(openai_chatgpt_account_id("not-a-jwt"), None);
    }
}
