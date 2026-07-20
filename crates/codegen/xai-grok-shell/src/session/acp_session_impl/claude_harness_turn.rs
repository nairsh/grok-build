//! Turn-execution mode that delegates a whole turn to the **Claude Agent SDK
//! harness** instead of the HTTP sampler. Selected by
//! [`claude_agent::should_use_claude_harness`] on the resolved model's sentinel
//! `base_url`; invoked from `handle_prompt` in place of the sampler loop, and
//! returning the same [`TurnOutcome`] so all surrounding turn bookkeeping is
//! preserved. See `crate::agent::claude_agent`.
//!
//! ⚠️ **UNVERIFIED.** This projection has not been exercised against a live
//! `claude` process or a real Pro/Max subscription (no login in CI), and the
//! workspace was not compiled in the authoring environment — expect a
//! compile-fix pass. Current fidelity gaps, tracked as follow-ups:
//!   * tool activity is rendered as transcript text, not native ACP tool cards;
//!   * permissions are handled by running the harness in `bypassPermissions`
//!     mode (its tools are sandboxed) rather than round-tripping through Atlas's
//!     interactive permission modal.

use super::*;

use crate::agent::claude_agent::{
    self, HarnessEvent, PermissionDecision,
    session::{ClaudeAgentSession, HarnessCommand},
};

impl SessionActor {
    /// Run one user turn through the Claude Agent SDK harness, projecting its
    /// stream-json events onto the ACP transcript. Returns a [`TurnOutcome`] so
    /// it slots into `handle_prompt`'s post-turn bookkeeping exactly like the
    /// sampler loop it replaces.
    pub(super) async fn run_claude_harness_turn(
        self: &Arc<Self>,
        user_message: &str,
    ) -> Result<TurnOutcome, acp::Error> {
        let model = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .filter(|m| !m.is_empty());
        let atlas_session_id = self.session_info.id.0.to_string();

        // Availability check first, for a friendly message rather than a raw
        // spawn error when `claude` is missing or not authenticated.
        let status = claude_agent::login::detect();
        if !matches!(status, claude_agent::login::HarnessStatus::Ready) {
            self.emit_harness_text(&claude_agent::login::status_hint(&status))
                .await;
            return Ok(harness_completed(Vec::new()));
        }

        let command = HarnessCommand {
            model,
            resume: claude_agent::recall_harness_session(&atlas_session_id),
            cwd: Some(self.session_info.cwd.to_string()),
            // Interactive round-trip through Atlas's permission modal is a
            // follow-up; until then the harness runs its own sandboxed tools.
            permission_mode: Some("bypassPermissions".to_owned()),
            partial_messages: false,
            extra_args: Vec::new(),
        };

        let mut session = match ClaudeAgentSession::spawn(&command) {
            Ok(session) => session,
            Err(err) => {
                self.emit_harness_text(&format!(
                    "Failed to start the Claude Agent SDK harness: {err}. {}",
                    claude_agent::login::status_hint(
                        &claude_agent::login::HarnessStatus::NotInstalled
                    )
                ))
                .await;
                return Ok(harness_completed(Vec::new()));
            }
        };

        if let Err(err) = session.send_user_message(user_message).await {
            self.emit_harness_text(&format!("Failed to send prompt to the harness: {err}"))
                .await;
            session.shutdown().await;
            return Ok(harness_completed(Vec::new()));
        }

        let mut tools_called: Vec<String> = Vec::new();
        while let Some(event) = session.next_event().await {
            match event {
                HarnessEvent::SessionStarted { session_id, .. } => {
                    if let Some(id) = session_id {
                        claude_agent::remember_harness_session(&atlas_session_id, &id);
                    }
                }
                HarnessEvent::AssistantText { text } => self.emit_harness_text(&text).await,
                HarnessEvent::Thinking { text } => self.emit_harness_thought(&text).await,
                HarnessEvent::ToolUse { name, input, .. } => {
                    tools_called.push(name.clone());
                    // TODO(parity): emit a native `acp::SessionUpdate::ToolCall`
                    // card instead of transcript text.
                    let rendered = serde_json::to_string(&input).unwrap_or_default();
                    self.emit_harness_text(&format!("\n⚙ {name} {rendered}\n"))
                        .await;
                }
                HarnessEvent::ToolResult {
                    content, is_error, ..
                } => {
                    let prefix = if is_error { "✗" } else { "→" };
                    self.emit_harness_text(&format!("{prefix} {content}\n"))
                        .await;
                }
                HarnessEvent::PermissionRequest(req) => {
                    // Only reached if a future change drops `bypassPermissions`.
                    // TODO(parity): route through the interactive permission modal.
                    let _ = session
                        .respond_permission(&req.request_id, &PermissionDecision::Allow, &req.input)
                        .await;
                }
                HarnessEvent::TurnResult {
                    is_error,
                    text,
                    session_id,
                    ..
                } => {
                    if let Some(id) = session_id {
                        claude_agent::remember_harness_session(&atlas_session_id, &id);
                    }
                    if is_error && let Some(text) = text {
                        self.emit_harness_text(&format!("\n[harness error] {text}\n"))
                            .await;
                    }
                    break;
                }
                HarnessEvent::Other(value) => {
                    tracing::debug!(target: "claude_agent", ?value, "unhandled harness event");
                }
            }
        }

        session.shutdown().await;
        Ok(harness_completed(tools_called))
    }

    /// Stream a chunk of assistant text into the transcript.
    async fn emit_harness_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.send_update(
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text.to_owned()),
            ))),
            None,
        )
        .await;
    }

    /// Stream a chunk of extended-thinking text into the transcript.
    async fn emit_harness_thought(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.send_update(
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text.to_owned()),
            ))),
            None,
        )
        .await;
    }
}

/// A minimal successful [`TurnOutcome`] for a completed harness turn.
fn harness_completed(tools_called: Vec<String>) -> TurnOutcome {
    TurnOutcome::Completed {
        snapshot: Box::new(None),
        tools_called,
        structured_output: None,
        refusal: false,
    }
}
