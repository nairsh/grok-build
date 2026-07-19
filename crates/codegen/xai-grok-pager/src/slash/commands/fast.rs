//! `/fast` — toggle the Codex provider's priority service tier.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Toggle Codex Fast mode for the current session.
pub struct FastCommand;

impl SlashCommand for FastCommand {
    fn name(&self) -> &str {
        "fast"
    }

    fn description(&self) -> &str {
        "Toggle Codex Fast mode (higher-priority responses)"
    }

    fn usage(&self) -> &str {
        "/fast"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        let Some(model_id) = ctx.models.current.clone() else {
            return CommandResult::Error("No active model".into());
        };
        if !ctx.models.current_supports_fast() {
            return CommandResult::Error(
                "Fast mode is available only for Codex provider models".into(),
            );
        }
        CommandResult::Action(Action::SwitchModel {
            model_id,
            // A same-model switch must retain the user's current effort.
            effort: ctx.models.reasoning_effort,
            service_tier_change: Some((!ctx.models.is_fast()).then(|| "priority".to_string())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::slash::commands::tests::make_ctx;
    use agent_client_protocol as acp;
    use std::sync::Arc;
    use xai_grok_shell::sampling::types::ReasoningEffort;

    fn codex_model() -> ModelState {
        let id = acp::ModelId::new(Arc::from("openai-codex/gpt-5.6-terra"));
        let info = acp::ModelInfo::new(id.clone(), "5.6 Terra".to_string()).meta(
            serde_json::json!({ "serviceTiers": [{ "id": "priority" }] })
                .as_object()
                .cloned(),
        );
        let mut models = ModelState::default();
        models.available.insert(id.clone(), info);
        models.current = Some(id);
        models.reasoning_effort = Some(ReasoningEffort::High);
        models
    }

    #[test]
    fn fast_enables_priority_without_changing_effort() {
        let models = codex_model();
        let mut ctx = make_ctx(&models);

        assert!(matches!(
            FastCommand.run(&mut ctx, ""),
            CommandResult::Action(Action::SwitchModel {
                effort: Some(ReasoningEffort::High),
                service_tier_change: Some(Some(ref tier)),
                ..
            }) if tier == "priority"
        ));
    }

    #[test]
    fn fast_disables_priority_when_already_selected() {
        let mut models = codex_model();
        models.set_service_tier(Some("priority".to_string()));
        let mut ctx = make_ctx(&models);

        assert!(matches!(
            FastCommand.run(&mut ctx, ""),
            CommandResult::Action(Action::SwitchModel {
                service_tier_change: Some(None),
                ..
            })
        ));
    }
}
