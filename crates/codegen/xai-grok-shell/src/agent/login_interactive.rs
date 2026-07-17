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

use tokio::io::{AsyncBufReadExt, BufReader};
use xai_grok_sampler::AuthScheme;
use xai_grok_sampling_types::ApiBackend;

use crate::agent::config::Config;
use crate::agent::connection::{ConnectionConfig, CredentialRef};
use crate::agent::credential_store::{Credential, CredentialStore, login_and_store};
use crate::agent::oauth_providers::SubscriptionProvider;

/// Show the interactive sign-in menu and run the chosen flow.
pub async fn run_interactive_login(config: &Config) -> anyhow::Result<()> {
    loop {
        println!("\nSelect a sign-in method:\n");
        println!("  1) xAI / Grok            (subscription sign-in)");
        println!("  2) Claude Pro/Max        (Anthropic subscription OAuth)");
        println!("  3) ChatGPT (Codex)       (OpenAI subscription OAuth)");
        println!("  4) GitHub Copilot        (device-code sign-in)");
        println!("  5) Add an API key        (any provider)");
        println!("  q) Cancel\n");

        let choice = prompt_line("Enter choice: ").await?;
        match choice.trim() {
            "1" => {
                crate::auth::run_cli_login(config, false, false, false).await?;
                return Ok(());
            }
            "2" => return subscription_login(SubscriptionProvider::Anthropic).await,
            "3" => return subscription_login(SubscriptionProvider::OpenAiCodex).await,
            "4" => {
                println!(
                    "\nGitHub Copilot uses a device-code flow that isn't wired into this menu \
                     yet.\nTrack it under credential id \"{}\".",
                    SubscriptionProvider::GithubCopilot.id()
                );
                return Ok(());
            }
            "5" => return add_api_key().await,
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
async fn add_api_key() -> anyhow::Result<()> {
    println!("\nSelect the API provider:\n");
    println!("  1) OpenAI");
    println!("  2) Anthropic");
    println!("  3) OpenRouter");
    println!("  4) Custom OpenAI-compatible endpoint\n");
    let choice = prompt_line("Enter choice: ").await?;
    let builtins = crate::agent::connection::builtin_connections();
    let (provider_id, mut connection) = match choice.trim() {
        "1" => ("openai".to_owned(), builtins["openai"].clone()),
        "2" => ("anthropic".to_owned(), builtins["anthropic"].clone()),
        "3" => ("openrouter".to_owned(), builtins["openrouter"].clone()),
        "4" => custom_connection().await?,
        _ => anyhow::bail!("unrecognized provider choice"),
    };

    let id = prompt_line(&format!(
        "Connection name [{provider_id}] (use a distinct name for another account): "
    ))
    .await?;
    let id = if id.trim().is_empty() {
        provider_id
    } else {
        id.trim().to_owned()
    };
    // Note: the key is echoed. For secret-free entry, prefer an env var or a
    // `credential = { api_key = "!command" }` in config.toml.
    let key = prompt_line("API key: ").await?;
    let key = key.trim().to_owned();
    anyhow::ensure!(!key.is_empty(), "API key must not be empty");
    let model = prompt_line("Model id (the exact provider model name): ").await?;
    let model = model.trim().to_owned();
    anyhow::ensure!(!model.is_empty(), "model id must not be empty");

    let path = CredentialStore::default_path();
    let mut store = CredentialStore::load(&path)?;
    store.put(id.clone(), Credential::ApiKey { key: key.clone() });
    store.save(&path)?;
    connection.credential = CredentialRef::Named(id.clone());
    if let Err(error) = save_provider_config(&id, &model, &connection) {
        store.remove(&id);
        let _ = store.save(&path);
        return Err(error);
    }
    println!(
        "\n✓ Connected \"{id}\" using model \"{model}\".\n  Run `atlas` to start, \
         or `atlas models` to verify the model list."
    );
    Ok(())
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

fn save_provider_config(
    connection_id: &str,
    model: &str,
    connection: &ConnectionConfig,
) -> anyhow::Result<()> {
    let path = xai_grok_config::grok_home().join("config.toml");
    save_provider_config_at(&path, connection_id, model, connection)
}

fn save_provider_config_at(
    path: &std::path::Path,
    connection_id: &str,
    model: &str,
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

    let catalog_id = format!("{connection_id}/{model}");
    let mut model_table = Table::new();
    model_table["connection"] = value(connection_id);
    model_table["model"] = value(model);
    document
        .entry("model")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config `model` must be a table"))?
        .insert(&catalog_id, Item::Table(model_table));

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
}
