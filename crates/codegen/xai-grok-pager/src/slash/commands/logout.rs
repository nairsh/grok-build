//! `/logout` -- manage and remove saved provider credentials.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

pub struct LogoutCommand;

impl SlashCommand for LogoutCommand {
    fn name(&self) -> &str {
        "logout"
    }

    fn description(&self) -> &str {
        "Manage and remove saved provider credentials"
    }

    fn usage(&self) -> &str {
        "/logout"
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenProviderLogout)
    }
}
