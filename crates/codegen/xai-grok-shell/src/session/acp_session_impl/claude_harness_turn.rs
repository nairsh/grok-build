//! Turn-execution mode that delegates a whole turn to the **Claude Agent SDK
//! harness** instead of the HTTP sampler. Selected by
//! [`claude_agent::should_use_claude_harness`] on the resolved model's sentinel
//! `base_url`; invoked from `handle_prompt` in place of the sampler loop, and
//! returning the same [`TurnOutcome`] so all surrounding turn bookkeeping is
//! preserved. See `crate::agent::claude_agent`.
//!
//! Harness tool activity is projected onto **native ACP tool cards** by
//! [`claude_agent::tool_render`], so a harness turn's Run / Read / Edit blocks
//! are indistinguishable from a sampler turn's.
//!
//! ⚠️ **UNVERIFIED.** This projection has not been exercised against a live
//! `claude` process or a real Pro/Max subscription (no login in CI). Remaining
//! fidelity gap, tracked as a follow-up:
//!   * permissions are handled by running the harness in `bypassPermissions`
//!     mode (its tools are sandboxed) rather than round-tripping through Atlas's
//!     interactive permission modal.

use super::*;

use crate::agent::claude_agent::{
    self, HarnessEvent, PermissionDecision,
    session::{ClaudeAgentSession, HarnessCommand},
    tool_render::{self, ToolCard},
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
        // Tool name + input, keyed by the harness's `tool_use_id`: the matching
        // `tool_result` frame carries neither, but both are needed to shape the
        // completion update the same way the start card was shaped.
        let mut open_tools: HashMap<String, (String, serde_json::Value)> = HashMap::new();
        while let Some(event) = session.next_event().await {
            match event {
                HarnessEvent::SessionStarted { session_id, .. } => {
                    if let Some(id) = session_id {
                        claude_agent::remember_harness_session(&atlas_session_id, &id);
                    }
                }
                HarnessEvent::AssistantText { text } => self.emit_harness_text(&text).await,
                HarnessEvent::Thinking { text } => self.emit_harness_thought(&text).await,
                HarnessEvent::ToolUse { id, name, input } => {
                    tools_called.push(name.clone());
                    self.emit_harness_tool_call(&id, &name, &input).await;
                    open_tools.insert(id, (name, input));
                }
                HarnessEvent::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    // A result for a tool_use we never saw (a frame lost to a
                    // reconnect) has no card to update; drop it rather than
                    // opening a second, contentless one.
                    if let Some((name, input)) = open_tools.remove(&tool_use_id) {
                        self.send_update(
                            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                                acp::ToolCallId::new(Arc::from(tool_use_id)),
                                tool_render::result_fields(
                                    &name,
                                    &input,
                                    &content,
                                    is_error,
                                    self.tool_context.cwd.as_path(),
                                ),
                            )),
                            None,
                        )
                        .await;
                    }
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

        // The turn ended (result frame, cancellation, or the harness exiting)
        // with tool cards still open. Close them: a card left `InProgress`
        // spins in the TUI for the rest of the session.
        for (tool_use_id, (name, _)) in open_tools {
            self.close_orphaned_tool_call(&tool_use_id, &name).await;
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

    /// Open a native tool card for a harness tool invocation.
    ///
    /// `InProgress` rather than `Pending`: the harness only reports a tool once
    /// it has already decided to run it, so there is no approval gap to show.
    /// A `TodoWrite` additionally drives the plan pane, which is where the TUI
    /// renders todos (its card is suppressed, exactly as on the sampler path).
    async fn emit_harness_tool_call(&self, id: &str, name: &str, input: &serde_json::Value) {
        let cwd = self.tool_context.cwd.as_path();
        let ToolCard {
            title,
            kind,
            raw_input,
            locations,
            mut content,
            diff_anchor,
        } = tool_render::tool_card(name, input, cwd);
        // Diffs are posted before the edit lands, so the file still holds the
        // pre-edit text the hunks are numbered against.
        if let Some(path) = diff_anchor
            && let Ok(text) = tokio::fs::read_to_string(&path).await
        {
            tool_render::anchor_diff_lines(&mut content, &text);
        }
        self.send_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(acp::ToolCallId::new(Arc::from(id)), title)
                    .kind(kind)
                    .status(acp::ToolCallStatus::InProgress)
                    .raw_input(Some(raw_input))
                    .locations(locations)
                    .content(content),
            ),
            None,
        )
        .await;
        if name == "TodoWrite"
            && let Some(plan) = tool_render::plan_update(input)
        {
            self.send_update(acp::SessionUpdate::Plan(plan), None).await;
        }
    }

    /// Fail a tool card whose result never arrived, so it stops rendering as
    /// running. The harness owns the tool, so whether it actually completed is
    /// unknowable here — the card says only that the outcome was not reported.
    async fn close_orphaned_tool_call(&self, tool_use_id: &str, name: &str) {
        tracing::debug!(
            target: "claude_agent",
            tool_use_id,
            tool_name = name,
            "harness turn ended before the tool reported a result"
        );
        self.send_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(Arc::from(tool_use_id)),
                acp::ToolCallUpdateFields::new()
                    .status(Some(acp::ToolCallStatus::Failed))
                    .content(Some(vec![acp::ToolCallContent::from(
                        acp::ContentBlock::Text(acp::TextContent::new(
                            "The turn ended before the harness reported this tool's result."
                                .to_owned(),
                        )),
                    )])),
            )),
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
