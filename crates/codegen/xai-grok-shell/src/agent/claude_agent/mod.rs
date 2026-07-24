//! Claude Agent SDK **harness backend** (Option A).
//!
//! Instead of pointing a wire adapter ([`ApiBackend`](xai_grok_sampling_types::ApiBackend))
//! at an HTTP endpoint and running Atlas's own agent loop, this backend
//! delegates a whole turn to the Claude Agent SDK — i.e. the `claude` runtime,
//! which *is* Claude Code packaged as a library. The SDK runs its own agent
//! loop with its own built-in tools, authenticated with the user's Claude
//! Pro/Max subscription (the same login the `claude` CLI uses). Atlas becomes
//! the front-end: it feeds user turns in and projects the harness's
//! `stream-json` events back onto the transcript, tool cards, permission
//! prompts, and session state.
//!
//! ## Why this is a separate execution mode, not a connection
//!
//! Atlas's [`connection`](crate::agent::connection) model resolves a model to
//! `(base_url, adapter, auth, headers)` and the sampler turns that into an HTTP
//! request whose tool loop Atlas drives. The Agent SDK is a *harness, not an
//! endpoint* — there is no request/response to adapt. So the routing decision
//! ([`should_use_claude_harness`]) happens **above** the sampler, and a turn
//! either goes to the HTTP sampler (every other provider) or to
//! [`session::ClaudeAgentSession`] (this backend). See the module docs on
//! [`session`] for the subprocess lifecycle.
//!
//! ## Compliance
//!
//! Using a Pro/Max subscription is licensed for Anthropic's own harness. This
//! backend qualifies because the Agent SDK *is* that harness — the agentic work
//! (tool calls, edits, the loop) is done by the SDK, not by Atlas's tools.
//! Authentication is delegated entirely to `claude login`; Atlas never mints or
//! holds a subscription token (contrast the removed `anthropic-subscription`
//! OAuth path).

pub mod login;
pub mod protocol;
pub mod session;
pub mod tool_render;

use serde_json::Value;

pub use protocol::{RawMessage, Usage};

/// Connection id that selects this backend. A model referencing
/// `connection = "claude-agent"` (or the built-in of the same name) is executed
/// through the harness rather than the HTTP sampler.
pub const CONNECTION_ID: &str = "claude-agent";

/// Sentinel `base_url` stamped onto harness-backed models by the built-in
/// `claude-agent` connection. It is never dialed — the turn loop detects it and
/// routes to the subprocess harness instead of the HTTP sampler. Using the
/// resolved `base_url` as the marker means detection survives model resolution
/// (which folds a connection's fields into the model and drops the connection
/// id) without threading a new field through `SamplingConfig`.
pub const HARNESS_BASE_URL: &str = "claudeagent://harness";

/// The provider-facing label shown in `/login` and the model picker.
pub const DISPLAY_NAME: &str = "Claude Agent SDK (subscription)";

/// A normalized harness event — the projection of a raw [`RawMessage`] into the
/// vocabulary the rest of Atlas consumes. This is the seam every UI surface
/// (transcript, tool cards, permission modal, session store) hangs off, so the
/// wire format in [`protocol`] can evolve without touching the front-end.
#[derive(Debug, Clone)]
pub enum HarnessEvent {
    /// Session bootstrapped; carries the id used for `--resume` continuity.
    SessionStarted {
        session_id: Option<String>,
        model: Option<String>,
        tools: Vec<String>,
    },
    /// A chunk of assistant-visible text (whole-message or partial delta).
    AssistantText { text: String },
    /// Extended-thinking text.
    Thinking { text: String },
    /// The harness invoked one of its built-in tools.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// Result of a harness tool call, fed back into the harness's own loop —
    /// Atlas renders it read-only (the harness, not Atlas, executed it).
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// The harness is asking permission to run a tool. Answer via
    /// [`session::ClaudeAgentSession::respond_permission`].
    PermissionRequest(PermissionRequest),
    /// Terminal summary of the turn.
    TurnResult {
        is_error: bool,
        text: Option<String>,
        session_id: Option<String>,
        usage: Option<Usage>,
        cost_usd: Option<f64>,
        num_turns: Option<u64>,
    },
    /// An unrecognized frame, preserved for logging/telemetry.
    Other(Value),
}

/// A pending `can_use_tool` permission prompt from the harness.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    /// Correlates the eventual [`PermissionDecision`] response.
    pub request_id: String,
    pub tool_name: String,
    pub input: Value,
}

/// The client's answer to a [`PermissionRequest`].
#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Allow,
    /// Deny, optionally telling the harness why so it can adapt.
    Deny {
        reason: Option<String>,
    },
}

/// Project a decoded wire message into zero or more [`HarnessEvent`]s. A single
/// assistant message can fan out into several events (text + one per tool_use),
/// which is why this returns a `Vec`.
pub fn project(raw: RawMessage) -> Vec<HarnessEvent> {
    use protocol::{ContentBlock, MessageContent};

    match raw {
        RawMessage::SystemInit(init) => vec![HarnessEvent::SessionStarted {
            session_id: init.session_id,
            model: init.model,
            tools: init.tools,
        }],
        RawMessage::Assistant(env) => {
            let mut out = Vec::new();
            match env.message.content {
                MessageContent::Text(text) => {
                    if !text.is_empty() {
                        out.push(HarnessEvent::AssistantText { text });
                    }
                }
                MessageContent::Blocks(blocks) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text } if !text.is_empty() => {
                                out.push(HarnessEvent::AssistantText { text })
                            }
                            ContentBlock::Thinking { thinking } if !thinking.is_empty() => {
                                out.push(HarnessEvent::Thinking { text: thinking })
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                out.push(HarnessEvent::ToolUse { id, name, input })
                            }
                            _ => {}
                        }
                    }
                }
            }
            out
        }
        RawMessage::User(env) => {
            // `user` frames carry tool results the harness produced internally.
            let mut out = Vec::new();
            if let MessageContent::Blocks(blocks) = env.message.content {
                for block in blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = block
                    {
                        out.push(HarnessEvent::ToolResult {
                            tool_use_id,
                            content: value_to_text(&content),
                            is_error,
                        });
                    }
                }
            }
            out
        }
        RawMessage::StreamEvent(value) => partial_text_event(&value)
            .map(|text| vec![HarnessEvent::AssistantText { text }])
            .unwrap_or_default(),
        RawMessage::Result(r) => vec![HarnessEvent::TurnResult {
            is_error: r.is_error,
            text: r.result,
            session_id: r.session_id,
            usage: r.usage,
            cost_usd: r.total_cost_usd,
            num_turns: r.num_turns,
        }],
        RawMessage::ControlRequest(req) => {
            if req.request.subtype.as_deref() == Some("can_use_tool") {
                vec![HarnessEvent::PermissionRequest(PermissionRequest {
                    request_id: req.request_id,
                    tool_name: req.request.tool_name.unwrap_or_default(),
                    input: req.request.input,
                })]
            } else {
                // Control sub-protocols we don't drive — preserve for logging.
                vec![HarnessEvent::Other(Value::String(format!(
                    "unhandled control_request: {:?}",
                    req.request.subtype
                )))]
            }
        }
        RawMessage::System(v) | RawMessage::ControlResponse(v) | RawMessage::Other(v) => {
            vec![HarnessEvent::Other(v)]
        }
    }
}

/// Extract incremental text from a `stream_event` partial-message frame
/// (`event.delta.text` for `content_block_delta`).
fn partial_text_event(value: &Value) -> Option<String> {
    let event = value.get("event")?;
    let delta = event.get("delta")?;
    let text = delta.get("text").and_then(Value::as_str)?;
    (!text.is_empty()).then(|| text.to_owned())
}

/// Render a tool-result `content` value (string, or array of `{type,text}`
/// blocks) into display text.
fn value_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        other => other.to_string(),
    }
}

/// Whether a resolved model routes to this harness backend rather than the HTTP
/// sampler, detected from its `base_url` ([`HARNESS_BASE_URL`]). Kept as a
/// single predicate so the one turn-dispatch branch that consults it stays
/// obvious and testable.
pub fn should_use_claude_harness(base_url: Option<&str>) -> bool {
    base_url == Some(HARNESS_BASE_URL)
}

/// Process-wide map from an Atlas session id to the harness's own `session_id`,
/// so successive turns in the same Atlas session resume the same `claude`
/// conversation (`--resume`). Kept here rather than as a `SessionActor` field to
/// avoid threading state through that type's large constructor; harness turns
/// are the only reader/writer.
fn resume_store() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static STORE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
    STORE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Record the harness `session_id` observed for an Atlas session.
pub fn remember_harness_session(atlas_session_id: &str, harness_session_id: &str) {
    if let Ok(mut map) = resume_store().lock() {
        map.insert(atlas_session_id.to_owned(), harness_session_id.to_owned());
    }
}

/// Recall the harness `session_id` to `--resume`, if a prior turn recorded one.
pub fn recall_harness_session(atlas_session_id: &str) -> Option<String> {
    resume_store().lock().ok()?.get(atlas_session_id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude_agent::protocol::parse_line;

    fn project_line(line: &str) -> Vec<HarnessEvent> {
        project(parse_line(line).unwrap().unwrap())
    }

    #[test]
    fn routes_only_on_the_sentinel_base_url() {
        assert!(should_use_claude_harness(Some(HARNESS_BASE_URL)));
        assert!(!should_use_claude_harness(Some(
            "https://api.anthropic.com/v1"
        )));
        assert!(!should_use_claude_harness(None));
    }

    #[test]
    fn assistant_message_fans_out_text_and_tools() {
        let events = project_line(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file":"a"}}]}}"#,
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], HarnessEvent::AssistantText { text } if text == "working"));
        assert!(matches!(&events[1], HarnessEvent::ToolUse { name, .. } if name == "Edit"));
    }

    #[test]
    fn user_frame_projects_tool_result() {
        let events = project_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}}"#,
        );
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], HarnessEvent::ToolResult { tool_use_id, content, is_error } if tool_use_id == "t1" && content == "ok" && !is_error)
        );
    }

    #[test]
    fn tool_result_array_content_is_flattened() {
        let events = project_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"line1"},{"type":"text","text":"line2"}]}]}}"#,
        );
        assert!(
            matches!(&events[0], HarnessEvent::ToolResult { content, .. } if content == "line1line2")
        );
    }

    #[test]
    fn permission_request_projects() {
        let events = project_line(
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#,
        );
        assert!(
            matches!(&events[0], HarnessEvent::PermissionRequest(p) if p.request_id == "r1" && p.tool_name == "Bash")
        );
    }

    #[test]
    fn partial_stream_delta_projects_text() {
        let events = project_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"chunk"}}}"#,
        );
        assert!(matches!(&events[0], HarnessEvent::AssistantText { text } if text == "chunk"));
    }

    #[test]
    fn resume_store_roundtrips() {
        let atlas = "atlas-session-roundtrip-xyz";
        assert_eq!(recall_harness_session(atlas), None);
        remember_harness_session(atlas, "harness-abc");
        assert_eq!(
            recall_harness_session(atlas).as_deref(),
            Some("harness-abc")
        );
    }

    #[test]
    fn session_started_carries_resume_id() {
        let events = project_line(
            r#"{"type":"system","subtype":"init","session_id":"sess-42","model":"claude-opus-4-8","tools":[]}"#,
        );
        assert!(
            matches!(&events[0], HarnessEvent::SessionStarted { session_id, .. } if session_id.as_deref() == Some("sess-42"))
        );
    }
}
