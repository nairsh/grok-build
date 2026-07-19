use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct WorkflowsCommand;

impl SlashCommand for WorkflowsCommand {
    fn name(&self) -> &str {
        "workflows"
    }

    fn aliases(&self) -> &[&str] {
        &["workflow"]
    }

    fn description(&self) -> &str {
        "Open the Dynamic Workflows control center"
    }

    fn usage(&self) -> &str {
        "/workflows"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn required_tools(&self) -> &[&str] {
        &["workflow"]
    }

    fn run(&self, ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session".into());
        }
        CommandResult::Action(Action::OpenWorkflows)
    }
}

pub struct UltracodeCommand;

impl SlashCommand for UltracodeCommand {
    fn name(&self) -> &str {
        "ultracode"
    }

    fn description(&self) -> &str {
        "Toggle automatic workflow orchestration with xhigh reasoning"
    }

    fn usage(&self) -> &str {
        "/ultracode [on|off]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn required_tools(&self) -> &[&str] {
        &["workflow"]
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        if ctx.session_id.is_none() {
            return CommandResult::Error("No active session".into());
        }
        let enabled = match args.trim().to_ascii_lowercase().as_str() {
            "" | "toggle" => None,
            "on" | "true" | "1" => Some(true),
            "off" | "false" | "0" => Some(false),
            _ => return CommandResult::Error("Usage: /ultracode [on|off]".into()),
        };
        CommandResult::Action(Action::SetUltracode(enabled))
    }
}
