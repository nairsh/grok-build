//! Interactive `atlas login` menu.
//!
//! Presents a simple numbered menu so the user can pick how to authenticate —
//! the xAI/Grok subscription, a third-party subscription (Claude Pro/Max,
//! ChatGPT Codex), or a stored API key — rather than remembering flags. Runs in
//! plain CLI context (before any TUI), so it uses line-based stdin prompts.
//!
//! Dispatches to the existing xAI flow ([`crate::auth::run_cli_login`]) and to
//! the provider-independent credential store
//! ([`crate::agent::credential_store`]).

use std::io::Write;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use xai_grok_sampler::AuthScheme;
use xai_grok_sampling_types::ApiBackend;

use crate::agent::config::Config;
use crate::agent::connection::{
    ApiKeyProviderPreset, ConnectionConfig, CredentialRef, api_key_provider_presets,
};
use crate::agent::credential_store::{Credential, CredentialStore, login_and_store};
use crate::agent::oauth_providers::SubscriptionProvider;

/// Show the interactive sign-in menu and run the chosen flow.
pub async fn run_interactive_login(config: &Config) -> anyhow::Result<()> {
    loop {
        println!("\nAtlas provider setup\n");
        println!("  1) Connect a subscription   xAI, Claude, or ChatGPT");
        println!("  2) Add an API key           26 built-in providers");
        println!("  3) Add a custom endpoint    OpenAI/Anthropic compatible");
        println!("  q) Cancel\n");

        let choice = prompt_line("Enter choice: ").await?;
        match choice.trim() {
            "1" => return subscription_menu(config).await,
            "2" => return api_key_menu().await,
            "3" => return add_api_key(None).await,
            "q" | "Q" | "" => {
                println!("Cancelled.");
                return Ok(());
            }
            other => {
                eprintln!("Unrecognized option {other:?}; try again.");
            }
        }
    }
}

/// Run setup for a named provider without walking the top-level menu. This is
/// used by `atlas login <provider>` and keeps scripting/autocomplete stable as
/// the interactive catalog grows.
pub async fn run_provider_login(config: &Config, provider: &str) -> anyhow::Result<()> {
    let normalized = provider.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "xai" | "grok" | "grok.com" => {
            crate::auth::run_cli_login(config, false, false, false).await
        }
        "claude" | "claude-subscription" | "anthropic-subscription" | "claude-agent" => {
            // The reverse-engineered Anthropic OAuth flow is retired. Claude
            // Pro/Max now runs through the Claude Agent SDK harness, whose auth
            // is delegated to `claude login` — print guidance instead of driving
            // an OAuth flow Atlas no longer owns.
            claude_agent_login_guidance()
        }
        "chatgpt" | "codex" | "openai-codex" => {
            subscription_login(SubscriptionProvider::OpenAiCodex).await
        }
        "github-copilot" | "copilot" => copilot_not_yet_available(),
        "custom" => add_api_key(None).await,
        _ => {
            let presets = api_key_provider_presets();
            let preset = presets.into_iter().find(|preset| {
                preset.id.eq_ignore_ascii_case(&normalized)
                    || preset.display_name.eq_ignore_ascii_case(provider.trim())
            });
            match preset {
                Some(preset) => add_api_key(Some(preset)).await,
                None => anyhow::bail!(
                    "unknown provider {provider:?}; run `atlas login` to see available providers"
                ),
            }
        }
    }
}

async fn subscription_menu(config: &Config) -> anyhow::Result<()> {
    println!("\nSubscriptions\n");
    println!("  1) xAI / Grok");
    println!("  2) Claude (Agent SDK — subscription)");
    println!("  3) ChatGPT Plus/Pro (Codex)");
    println!("  4) GitHub Copilot");
    println!("  q) Cancel\n");
    match prompt_line("Enter choice: ").await?.trim() {
        "1" => crate::auth::run_cli_login(config, false, false, false).await,
        "2" => claude_agent_login_guidance(),
        "3" => subscription_login(SubscriptionProvider::OpenAiCodex).await,
        "4" => copilot_not_yet_available(),
        "q" | "Q" | "" => {
            println!("Cancelled.");
            Ok(())
        }
        _ => anyhow::bail!("unrecognized subscription choice"),
    }
}

fn copilot_not_yet_available() -> anyhow::Result<()> {
    anyhow::bail!(
        "GitHub Copilot's device-code transport is not implemented yet; \
         use another provider for now"
    )
}

/// Guide the user to authenticate the Claude Agent SDK harness. Auth is owned by
/// the `claude` runtime, not Atlas — so we print its status and the `claude
/// login` command rather than driving an OAuth flow ourselves.
fn claude_agent_login_guidance() -> anyhow::Result<()> {
    use crate::agent::claude_agent::login;
    let status = login::detect();
    println!("\nClaude Agent SDK\n");
    println!("  {}", login::status_hint(&status));
    match status {
        login::HarnessStatus::Ready => {
            println!(
                "\n  Run `atlas` and pick the \"{}\" entry from `/model` \
                 (restart atlas first if it was already running).",
                crate::agent::claude_agent::DISPLAY_NAME
            );
        }
        _ => {
            println!("\n  Then select a model on the `claude-agent` connection.");
        }
    }
    Ok(())
}

async fn api_key_menu() -> anyhow::Result<()> {
    let presets = api_key_provider_presets();
    println!("\nAPI-key providers\n");
    for (index, preset) in presets.iter().enumerate() {
        let configured = std::env::var(preset.env_key)
            .ok()
            .is_some_and(|value| !value.trim().is_empty());
        println!(
            "  {:>2}) {:<34} {}{}",
            index + 1,
            preset.display_name,
            preset.env_key,
            if configured { "  ✓ env set" } else { "" },
        );
    }
    println!("   b) Back\n");
    let choice = prompt_line("Provider number or name: ").await?;
    let choice = choice.trim();
    if matches!(choice, "" | "b" | "B") {
        println!("Cancelled.");
        return Ok(());
    }
    let preset = resolve_preset_choice(&presets, choice)
        .ok_or_else(|| anyhow::anyhow!("unknown provider {choice:?}"))?;
    add_api_key(Some(preset.clone())).await
}

fn resolve_preset_choice<'a>(
    presets: &'a [ApiKeyProviderPreset],
    choice: &str,
) -> Option<&'a ApiKeyProviderPreset> {
    if let Ok(number) = choice.parse::<usize>() {
        return number.checked_sub(1).and_then(|index| presets.get(index));
    }
    presets.iter().find(|preset| {
        preset.id.eq_ignore_ascii_case(choice) || preset.display_name.eq_ignore_ascii_case(choice)
    })
}

/// Run a subscription OAuth flow and persist the tokens to the credential store.
async fn subscription_login(provider: SubscriptionProvider) -> anyhow::Result<()> {
    let path = CredentialStore::default_path();
    let client = reqwest::Client::new();
    println!(
        "\nStarting {} sign-in. A browser window will open; complete the login there.",
        provider.display_name()
    );
    let id = login_and_store(provider, &path, &client, true).await?;
    if provider == SubscriptionProvider::OpenAiCodex {
        match crate::agent::provider_catalog::refresh_openai_codex_catalog_if_stale(true).await {
            Ok(count) if count > 0 => {
                println!("\n✓ Signed in and loaded {count} available ChatGPT models.");
            }
            Ok(_) => println!("\n✓ Signed in. Atlas will use its bundled ChatGPT model fallback."),
            Err(error) => {
                eprintln!(
                    "\nSigned in, but model discovery failed ({error}). \
                     Atlas will use its cached or bundled ChatGPT models."
                );
            }
        }
    } else {
        println!("\n✓ Signed in. Saved credential \"{id}\".");
    }
    Ok(())
}

/// Prompt for a provider, credential, and model, then persist a complete usable
/// connection. A saved key without a connection/model is deliberately avoided:
/// it would look connected but could never route a prompt.
async fn add_api_key(preset: Option<ApiKeyProviderPreset>) -> anyhow::Result<()> {
    let (provider_id, default_model, mut connection) = match preset {
        Some(preset) => (
            preset.id.to_owned(),
            Some(preset.default_model),
            preset.connection,
        ),
        None => {
            let (id, connection) = custom_connection().await?;
            (id, None, connection)
        }
    };

    let id = prompt_line(&format!(
        "Connection name [{provider_id}] (use a distinct name for another account): "
    ))
    .await?;
    let id = if id.trim().is_empty() {
        provider_id.clone()
    } else {
        id.trim().to_owned()
    };
    if provider_id == "litellm" {
        let default_url = connection.base_url.as_deref().unwrap_or_default();
        let base_url = prompt_line(&format!("LiteLLM API base URL [{default_url}]: ")).await?;
        if !base_url.trim().is_empty() {
            connection.base_url = Some(base_url.trim().trim_end_matches('/').to_owned());
        }
    }
    let key = prompt_secret("API key (input hidden): ").await?;
    let key = key.trim().to_owned();
    anyhow::ensure!(!key.is_empty(), "API key must not be empty");
    // OpenRouter's public catalog is intentionally not fetched: it is far too
    // large for a useful local model picker. Messages-adapter connections
    // (Anthropic, Bedrock) have no `/models` discovery endpoint at all. Both
    // ask for the explicit subset the user wants to enable instead.
    let openrouter = provider_id == "openrouter";
    let manual_multi_model =
        openrouter || !crate::agent::connection::supports_model_discovery(&connection);
    let discovered_models = if !manual_multi_model {
        match discover_openai_models(&connection, &key).await {
            Ok(models) if !models.is_empty() => {
                println!("\nFound {} models at this endpoint.", models.len());
                models
            }
            Ok(_) => Vec::new(),
            Err(error) => {
                eprintln!("\nCould not fetch /models ({error}); enter a model id manually.");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let discovered_default = discovered_models.first().map(String::as_str);
    let model_prompt = if manual_multi_model {
        match default_model {
            Some(default) => format!("Model ids (comma-separated) [{default}]: "),
            None => "Model ids (comma-separated): ".to_owned(),
        }
    } else {
        match default_model {
            Some(default) => format!("Model id [{}]: ", discovered_default.unwrap_or(default)),
            None if discovered_default.is_some() => {
                format!(
                    "Model id [{}]: ",
                    discovered_default.expect("checked above")
                )
            }
            None => "Model id (the exact provider model name): ".to_owned(),
        }
    };
    let model = prompt_line(&model_prompt).await?;
    let model = match (model.trim(), discovered_default.or(default_model)) {
        ("", Some(default)) => default.to_owned(),
        ("", None) => String::new(),
        (value, _) => value.to_owned(),
    };
    let models = if manual_multi_model {
        let mut seen = std::collections::HashSet::new();
        let requested = model
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .filter(|id| seen.insert((*id).to_owned()))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        anyhow::ensure!(!requested.is_empty(), "at least one model id is required");
        requested
    } else {
        anyhow::ensure!(!model.is_empty(), "model id must not be empty");
        if discovered_models.is_empty() {
            vec![model.clone()]
        } else {
            discovered_models
        }
    };

    save_api_key_connection(&id, &key, &models, connection)?;
    println!(
        "\n✓ Connected \"{id}\" using {} model(s).\n  Run `atlas` to start, \
         or `atlas models` to verify the model list.",
        models.len()
    );
    Ok(())
}

/// Persist an API-key credential and its usable model connection.
///
/// Shared by the pre-TUI `atlas login` flow and the native `/login` modal so
/// both routes produce identical credential and config files. The models are
/// supplied by the caller: the CLI may use `/models` discovery while the TUI
/// accepts a direct model id without ever putting the secret on a command line.
pub fn save_api_key_connection(
    connection_id: &str,
    api_key: &str,
    models: &[String],
    mut connection: ConnectionConfig,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !connection_id.trim().is_empty(),
        "connection name must not be empty"
    );
    anyhow::ensure!(!api_key.trim().is_empty(), "API key must not be empty");
    anyhow::ensure!(!models.is_empty(), "at least one model is required");

    let path = CredentialStore::default_path();
    let mut store = CredentialStore::load(&path)?;
    store.put(
        connection_id,
        Credential::ApiKey {
            key: api_key.to_owned(),
        },
    );
    store.save(&path)?;
    connection.credential = CredentialRef::Named(connection_id.to_owned());
    if let Err(error) = save_provider_config_models(connection_id, models, &connection) {
        store.remove(connection_id);
        let _ = store.save(&path);
        return Err(error);
    }
    Ok(())
}

/// Native-TUI convenience wrapper around [`save_api_key_connection`].
/// The pager depends only on the shell crate, so keep protocol details here
/// instead of duplicating internal sampler dependencies in the UI crate.
pub fn save_api_key_connection_for_provider(
    provider_id: &str,
    connection_id: &str,
    api_key: &str,
    models: &[String],
    base_url: Option<String>,
) -> anyhow::Result<()> {
    let preset = api_key_provider_presets()
        .into_iter()
        .find(|preset| preset.id == provider_id);
    let mut connection = match preset {
        Some(preset) => preset.connection,
        None => ConnectionConfig {
            adapter: Some(ApiBackend::ChatCompletions),
            base_url: base_url.clone(),
            ..Default::default()
        },
    };
    if let Some(base_url) = base_url {
        let base_url = base_url.trim().trim_end_matches('/');
        anyhow::ensure!(!base_url.is_empty(), "API base URL must not be empty");
        connection.base_url = Some(base_url.to_owned());
    }
    save_api_key_connection(connection_id, api_key, models, connection)
}

/// Remove a saved credential and any generated connection/model entries that
/// reference an API key by the same id. OAuth credentials do not generate
/// config entries, so logging out of them only updates the credential store.
pub fn remove_saved_credential(credential_id: &str) -> anyhow::Result<()> {
    let credential_path = CredentialStore::default_path();
    let config_path = xai_grok_config::grok_home().join("config.toml");
    remove_saved_credential_at(&credential_path, &config_path, credential_id)
}

fn remove_saved_credential_at(
    credential_path: &std::path::Path,
    config_path: &std::path::Path,
    credential_id: &str,
) -> anyhow::Result<()> {
    let mut store = CredentialStore::load(credential_path)?;
    let removed = store
        .remove(credential_id)
        .ok_or_else(|| anyhow::anyhow!("credential no longer exists"))?;
    store.save(credential_path)?;

    if matches!(removed, Credential::ApiKey { .. })
        && let Err(error) = remove_provider_config_at(config_path, credential_id)
    {
        store.put(credential_id, removed);
        if let Err(rollback_error) = store.save(credential_path) {
            return Err(anyhow::anyhow!(
                "could not update config: {error}; restoring the credential also failed: {rollback_error}"
            ));
        }
        return Err(error);
    }
    Ok(())
}

/// Complete a subscription login from an embedded UI without requiring that UI
/// to depend on `reqwest` or know the credential-store layout.
pub async fn login_subscription_and_store(provider: SubscriptionProvider) -> anyhow::Result<()> {
    let path = CredentialStore::default_path();
    let client = reqwest::Client::new();
    login_and_store(provider, &path, &client, true).await?;
    if provider == SubscriptionProvider::OpenAiCodex
        && let Err(error) =
            crate::agent::provider_catalog::refresh_openai_codex_catalog_if_stale(true).await
    {
        tracing::warn!(%error, "ChatGPT model discovery failed after browser login");
    }
    Ok(())
}

#[derive(Deserialize)]
struct OpenAiModelsResponse {
    #[serde(default)]
    data: Vec<OpenAiModel>,
}

#[derive(Deserialize)]
struct OpenAiModel {
    id: String,
}

/// Fetch models from an OpenAI-compatible endpoint (including LiteLLM's
/// proxy). Discovery is intentionally best-effort: private gateways commonly
/// disable this endpoint, in which case the normal manual model prompt remains
/// available.
async fn discover_openai_models(
    connection: &ConnectionConfig,
    api_key: &str,
) -> anyhow::Result<Vec<String>> {
    let base_url = connection
        .base_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("connection has no base URL"))?;
    discover_openai_models_at(base_url, api_key).await
}

/// Fetch the complete model catalog from an OpenAI-compatible endpoint.
///
/// Used by both the line-based login command and the native TUI. `base_url`
/// is the API root (for example `http://localhost:4000/v1`); this function
/// requests its standard `/models` resource.
pub async fn discover_openai_models_at(
    base_url: &str,
    api_key: &str,
) -> anyhow::Result<Vec<String>> {
    let base_url = base_url.trim().trim_end_matches('/');
    anyhow::ensure!(!base_url.is_empty(), "API base URL must not be empty");
    anyhow::ensure!(!api_key.trim().is_empty(), "API key must not be empty");
    let client = reqwest::Client::new();
    let direct_url = format!("{base_url}/models");
    let response = client.get(&direct_url).bearer_auth(api_key).send().await?;
    // LiteLLM is commonly configured with either the API root
    // (`http://host:4000`) or the OpenAI-compatible root
    // (`http://host:4000/v1`). Support both without making the user know
    // which form their gateway expects.
    let response = if response.status() == reqwest::StatusCode::NOT_FOUND
        && !base_url.trim_end_matches('/').ends_with("/v1")
    {
        client
            .get(format!("{base_url}/v1/models"))
            .bearer_auth(api_key)
            .send()
            .await?
    } else {
        response
    }
    .error_for_status()?;
    let mut models = response
        .json::<OpenAiModelsResponse>()
        .await?
        .data
        .into_iter()
        .map(|model| model.id)
        .filter(|id| !id.trim().is_empty())
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    Ok(models)
}

async fn custom_connection() -> anyhow::Result<(String, ConnectionConfig)> {
    let base_url = prompt_line("API base URL (for example http://localhost:11434/v1): ").await?;
    let base_url = base_url.trim().trim_end_matches('/').to_owned();
    anyhow::ensure!(!base_url.is_empty(), "base URL must not be empty");
    println!("\nWire protocol:\n  1) Chat Completions\n  2) Responses\n  3) Anthropic Messages");
    let adapter = match prompt_line("Enter choice [1]: ").await?.trim() {
        "" | "1" => ApiBackend::ChatCompletions,
        "2" => ApiBackend::Responses,
        "3" => ApiBackend::Messages,
        _ => anyhow::bail!("unrecognized wire protocol"),
    };
    let auth_scheme = matches!(&adapter, ApiBackend::Messages).then_some(AuthScheme::XApiKey);
    Ok((
        "custom".to_owned(),
        ConnectionConfig {
            adapter: Some(adapter),
            base_url: Some(base_url),
            auth_scheme,
            ..Default::default()
        },
    ))
}

fn save_provider_config_models(
    connection_id: &str,
    models: &[String],
    connection: &ConnectionConfig,
) -> anyhow::Result<()> {
    let path = xai_grok_config::grok_home().join("config.toml");
    save_provider_config_models_at(&path, connection_id, models, connection)
}

fn save_provider_config_at(
    path: &std::path::Path,
    connection_id: &str,
    model: &str,
    connection: &ConnectionConfig,
) -> anyhow::Result<()> {
    save_provider_config_models_at(path, connection_id, &[model.to_owned()], connection)
}

fn save_provider_config_models_at(
    path: &std::path::Path,
    connection_id: &str,
    models: &[String],
    connection: &ConnectionConfig,
) -> anyhow::Result<()> {
    use toml_edit::{DocumentMut, InlineTable, Item, Table, Value, value};

    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut document: DocumentMut = content.parse().map_err(|error| {
        anyhow::anyhow!("config.toml is invalid; refusing to overwrite it: {error}")
    })?;

    let mut connection_table = Table::new();
    if let Some(adapter) = &connection.adapter {
        connection_table["adapter"] = value(match adapter {
            ApiBackend::ChatCompletions => "chat_completions",
            ApiBackend::Responses => "responses",
            ApiBackend::Messages => "messages",
        });
    }
    if let Some(base_url) = &connection.base_url {
        connection_table["base_url"] = value(base_url);
    }
    if let Some(api_base_url) = &connection.api_base_url {
        connection_table["api_base_url"] = value(api_base_url);
    }
    if let Some(auth_scheme) = connection.auth_scheme {
        connection_table["auth_scheme"] = value(match auth_scheme {
            AuthScheme::Bearer => "bearer",
            AuthScheme::XApiKey => "x_api_key",
        });
    }
    if !connection.extra_headers.is_empty() {
        let mut headers = Table::new();
        for (name, header_value) in &connection.extra_headers {
            headers[name.as_str()] = value(header_value);
        }
        connection_table["extra_headers"] = Item::Table(headers);
    }
    let mut credential = InlineTable::new();
    credential.insert("named", Value::from(connection_id));
    connection_table["credential"] = Item::Value(Value::InlineTable(credential));
    document
        .entry("connection")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config `connection` must be a table"))?
        .insert(connection_id, Item::Table(connection_table));

    let model_tables = document
        .entry("model")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config `model` must be a table"))?;
    for model in models {
        let catalog_id = format!("{connection_id}/{model}");
        let mut model_table = Table::new();
        model_table["connection"] = value(connection_id);
        model_table["model"] = value(model);
        model_tables.insert(&catalog_id, Item::Table(model_table));
    }

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    std::fs::write(&temporary, document.to_string())?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn remove_provider_config_at(path: &std::path::Path, credential_id: &str) -> anyhow::Result<()> {
    use toml_edit::{DocumentMut, Item};

    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut document: DocumentMut = content.parse().map_err(|error| {
        anyhow::anyhow!("config.toml is invalid; refusing to overwrite it: {error}")
    })?;
    let remove_connection = document
        .get("connection")
        .and_then(Item::as_table)
        .and_then(|connections| connections.get(credential_id))
        .and_then(Item::as_table)
        .and_then(|connection| connection.get("credential"))
        .and_then(Item::as_value)
        .and_then(toml_edit::Value::as_inline_table)
        .and_then(|credential| credential.get("named"))
        .and_then(toml_edit::Value::as_str)
        == Some(credential_id);

    let mut changed = false;
    if remove_connection
        && let Some(connections) = document.get_mut("connection").and_then(Item::as_table_mut)
    {
        connections.remove(credential_id);
        changed = true;
    }

    if let Some(models) = document.get_mut("model").and_then(Item::as_table_mut) {
        let generated_models = models
            .iter()
            .filter_map(|(id, item)| {
                let connection = item.as_table()?.get("connection").and_then(Item::as_str)?;
                (connection == credential_id).then(|| id.to_owned())
            })
            .collect::<Vec<_>>();
        for id in generated_models {
            models.remove(&id);
            changed = true;
        }
    }

    if !changed {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    std::fs::write(&temporary, document.to_string())?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

/// Print `prompt` (no newline) and read one line from stdin.
async fn prompt_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    let mut reader = BufReader::new(tokio::io::stdin());
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        // EOF (e.g. piped/non-interactive): treat as cancel.
        anyhow::bail!("no input (stdin closed); run in an interactive terminal");
    }
    Ok(line)
}

/// Read a secret without echoing it when stdin is a Unix terminal. The
/// terminal settings are restored by a guard even when reading fails.
async fn prompt_secret(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let line = tokio::task::spawn_blocking(read_secret_line)
        .await
        .map_err(|error| anyhow::anyhow!("secret input task failed: {error}"))??;
    println!();
    Ok(line)
}

#[cfg(unix)]
fn read_secret_line() -> std::io::Result<String> {
    use nix::sys::termios::{LocalFlags, SetArg, Termios, tcgetattr, tcsetattr};

    struct EchoGuard {
        stdin: std::io::Stdin,
        original: Option<Termios>,
    }
    impl Drop for EchoGuard {
        fn drop(&mut self) {
            if let Some(original) = self.original.as_ref() {
                let _ = tcsetattr(&self.stdin, SetArg::TCSANOW, original);
            }
        }
    }

    let stdin = std::io::stdin();
    let original = tcgetattr(&stdin).ok();
    if let Some(mut hidden) = original.clone() {
        hidden.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(&stdin, SetArg::TCSANOW, &hidden).map_err(std::io::Error::other)?;
    }
    let guard = EchoGuard { stdin, original };
    let mut line = String::new();
    guard.stdin.read_line(&mut line)?;
    Ok(line)
}

#[cfg(not(unix))]
fn read_secret_line() -> std::io::Result<String> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_provider_preserves_config_and_creates_a_routable_model() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "[ui]\nshow_timestamps = true\n").unwrap();
        let mut connection = crate::agent::connection::builtin_connections()["openai"].clone();
        connection.credential = CredentialRef::Named("openai-work".to_owned());

        save_provider_config_at(&path, "openai-work", "gpt-test", &connection).unwrap();

        let raw: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["ui"]["show_timestamps"].as_bool(), Some(true));
        let cfg = Config::new_from_toml_cfg(&raw).unwrap();
        assert!(cfg.connections.contains_key("openai-work"));
        let model = cfg
            .config_models
            .get("openai-work/gpt-test")
            .expect("model is configured");
        assert_eq!(model.connection.as_deref(), Some("openai-work"));
        assert_eq!(model.model.as_deref(), Some("gpt-test"));
    }

    #[test]
    fn removing_api_key_credential_cleans_generated_config_entries() {
        let directory = tempfile::tempdir().unwrap();
        let credential_path = directory.path().join("credentials.json");
        let config_path = directory.path().join("config.toml");
        std::fs::write(&config_path, "[ui]\nshow_timestamps = true\n").unwrap();
        let mut connection = crate::agent::connection::builtin_connections()["openai"].clone();
        connection.credential = CredentialRef::Named("openai-work".to_owned());
        save_provider_config_models_at(
            &config_path,
            "openai-work",
            &["gpt-one".to_owned(), "gpt-two".to_owned()],
            &connection,
        )
        .unwrap();
        let mut store = CredentialStore::default();
        store.put(
            "openai-work",
            Credential::ApiKey {
                key: "secret".to_owned(),
            },
        );
        store.save(&credential_path).unwrap();

        remove_saved_credential_at(&credential_path, &config_path, "openai-work").unwrap();

        let store = CredentialStore::load(&credential_path).unwrap();
        assert!(store.get("openai-work").is_none());
        let raw: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(raw["ui"]["show_timestamps"].as_bool(), Some(true));
        assert!(
            raw.get("connection")
                .and_then(|value| value.get("openai-work"))
                .is_none()
        );
        assert!(
            raw.get("model")
                .and_then(toml::Value::as_table)
                .is_none_or(toml::map::Map::is_empty)
        );
    }

    #[test]
    fn provider_choices_accept_number_id_and_display_name() {
        let presets = api_key_provider_presets();
        assert_eq!(
            resolve_preset_choice(&presets, "1").map(|preset| preset.id),
            presets.first().map(|preset| preset.id)
        );
        assert_eq!(
            resolve_preset_choice(&presets, "openrouter").map(|preset| preset.id),
            Some("openrouter")
        );
        assert_eq!(
            resolve_preset_choice(&presets, "Google Gemini").map(|preset| preset.id),
            Some("google")
        );
        assert!(resolve_preset_choice(&presets, "not-a-provider").is_none());
    }
}
