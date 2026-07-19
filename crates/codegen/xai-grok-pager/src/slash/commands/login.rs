//! `/login` -- connect or re-authenticate an AI provider.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }

    fn description(&self) -> &str {
        "Connect or re-authenticate an AI provider"
    }

    fn usage(&self) -> &str {
        "/login [provider]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[provider]")
    }

    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        let mut items = vec![
            item("xai", "xAI / Grok subscription", "Browser or device login"),
            item(
                "anthropic-subscription",
                "Claude Pro/Max subscription",
                "Browser login",
            ),
            item("openai-codex", "ChatGPT Plus/Pro (Codex)", "Browser login"),
            item(
                "github-copilot",
                "GitHub Copilot",
                "Device login (transport pending)",
            ),
        ];
        items.extend(
            xai_grok_shell::agent::connection::api_key_provider_presets()
                .into_iter()
                .map(|preset| ArgItem {
                    display: preset.display_name.to_owned(),
                    match_text: format!(
                        "{} {} {} api key",
                        preset.id, preset.display_name, preset.env_key
                    ),
                    insert_text: preset.id.to_owned(),
                    description: format!("API key · {}", preset.env_key),
                }),
        );
        items.push(item(
            "custom",
            "Custom compatible endpoint",
            "Choose endpoint, protocol, and model",
        ));
        Some(items)
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let provider = args.trim();
        CommandResult::Action(Action::OpenProviderLogin {
            provider: (!provider.is_empty()).then(|| provider.to_owned()),
        })
    }
}

fn item(id: &str, display: &str, description: &str) -> ArgItem {
    ArgItem {
        display: display.to_owned(),
        match_text: format!("{id} {display}"),
        insert_text: id.to_owned(),
        description: description.to_owned(),
    }
}
