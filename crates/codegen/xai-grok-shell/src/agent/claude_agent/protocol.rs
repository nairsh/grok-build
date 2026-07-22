//! Wire types for the Claude Agent SDK's `stream-json` protocol.
//!
//! The `claude` CLI (which the Claude Agent SDK wraps) emits one JSON object per
//! line when run with `--output-format stream-json`. Each object is tagged by a
//! top-level `type` field. This module models the subset of that protocol Atlas
//! consumes, deliberately tolerant of unknown fields and unknown message types
//! so a `claude` upgrade never hard-fails the integration — unrecognized shapes
//! surface as [`RawMessage::Other`] and are logged, not dropped on the floor.
//!
//! Protocol reference: `claude --output-format stream-json --input-format
//! stream-json` (the same framing the TypeScript/Python Agent SDKs speak). This
//! is **not** the Anthropic Messages wire protocol — it is the harness protocol,
//! which is why it lives here and not behind an [`ApiBackend`](xai_grok_sampling_types::ApiBackend).

use serde::Deserialize;
use serde_json::Value;

/// A single decoded `stream-json` line.
///
/// Parsed leniently: the concrete variants cover what the turn projection needs;
/// anything else (new message types, control sub-protocols we don't drive)
/// lands in [`RawMessage::Other`] with the original JSON preserved.
#[derive(Debug, Clone)]
pub enum RawMessage {
    /// `{"type":"system","subtype":"init",...}` — emitted once at session start.
    SystemInit(SystemInit),
    /// Any other `system` subtype (e.g. compaction notices) — kept raw.
    System(Value),
    /// `{"type":"assistant","message":{...}}` — a full assistant turn.
    Assistant(ApiMessageEnvelope),
    /// `{"type":"user","message":{...}}` — tool results the harness fed back.
    User(ApiMessageEnvelope),
    /// `{"type":"stream_event","event":{...}}` — partial deltas when the harness
    /// is run with `--include-partial-messages`.
    StreamEvent(Value),
    /// `{"type":"result",...}` — terminal turn summary (cost, usage, status).
    Result(ResultMessage),
    /// `{"type":"control_request",...}` — the harness asking the client for a
    /// decision (notably `can_use_tool` permission prompts).
    ControlRequest(ControlRequest),
    /// `{"type":"control_response",...}` — acknowledgement of a control request
    /// we sent (interrupt, initialize).
    ControlResponse(Value),
    /// Anything unrecognized. Preserved verbatim so callers can log it.
    Other(Value),
}

/// `system`/`init` payload. Only the fields Atlas surfaces are typed.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SystemInit {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default)]
    pub permission_mode: Option<String>,
}

/// Envelope around an Anthropic-style `Message` carried by `assistant`/`user`
/// lines.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiMessageEnvelope {
    #[serde(default)]
    pub session_id: Option<String>,
    pub message: ApiMessage,
}

/// The Anthropic `Message` object as embedded in the harness stream.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ApiMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub content: MessageContent,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// `content` is either a bare string or an array of typed blocks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl Default for MessageContent {
    fn default() -> Self {
        MessageContent::Blocks(Vec::new())
    }
}

/// A single content block within a message.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ToolResult {
        #[serde(default)]
        tool_use_id: String,
        #[serde(default)]
        content: Value,
        #[serde(default)]
        is_error: bool,
    },
    /// Forward-compat: server_tool_use, redacted_thinking, image, etc.
    #[serde(other)]
    Unknown,
}

/// Terminal `result` line for a turn.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ResultMessage {
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub is_error: bool,
    /// Final assistant text, when the harness includes it.
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    #[serde(default)]
    pub num_turns: Option<u64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// Token usage, mirrored from the Anthropic `usage` object.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
}

/// A `control_request` from the harness. The only subtype Atlas actively drives
/// is `can_use_tool`; others are surfaced raw so the caller can NAK them.
#[derive(Debug, Clone, Deserialize)]
pub struct ControlRequest {
    pub request_id: String,
    #[serde(default)]
    pub request: ControlRequestBody,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ControlRequestBody {
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub input: Value,
    /// Passed through untouched on non-`can_use_tool` subtypes.
    #[serde(flatten)]
    pub extra: Value,
}

/// Decode one line of `stream-json`. Blank lines yield `None`; malformed JSON is
/// an error the caller should log-and-continue on (a single bad frame must not
/// tear down the turn).
pub fn parse_line(line: &str) -> Result<Option<RawMessage>, serde_json::Error> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(line)?;
    let ty = value.get("type").and_then(Value::as_str).unwrap_or("");
    let msg = match ty {
        "system" => {
            let subtype = value.get("subtype").and_then(Value::as_str);
            if subtype == Some("init") {
                RawMessage::SystemInit(SystemInit::deserialize(&value).unwrap_or_default())
            } else {
                RawMessage::System(value)
            }
        }
        "assistant" => match ApiMessageEnvelope::deserialize(&value) {
            Ok(env) => RawMessage::Assistant(env),
            Err(_) => RawMessage::Other(value),
        },
        "user" => match ApiMessageEnvelope::deserialize(&value) {
            Ok(env) => RawMessage::User(env),
            Err(_) => RawMessage::Other(value),
        },
        "stream_event" => RawMessage::StreamEvent(value),
        "result" => RawMessage::Result(ResultMessage::deserialize(&value).unwrap_or_default()),
        "control_request" => match ControlRequest::deserialize(&value) {
            Ok(req) => RawMessage::ControlRequest(req),
            Err(_) => RawMessage::Other(value),
        },
        "control_response" => RawMessage::ControlResponse(value),
        _ => RawMessage::Other(value),
    };
    Ok(Some(msg))
}

/// Flatten a [`MessageContent`] into the plain assistant text it contains,
/// concatenating text blocks and ignoring tool-use/thinking blocks.
pub fn message_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => {
            let mut out = String::new();
            for b in blocks {
                if let ContentBlock::Text { text } = b {
                    out.push_str(text);
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_line_is_none() {
        assert!(parse_line("   ").unwrap().is_none());
    }

    #[test]
    fn parses_system_init() {
        let line = r#"{"type":"system","subtype":"init","session_id":"sess-1","model":"claude-opus-4-8","tools":["Bash","Edit"],"permission_mode":"default"}"#;
        match parse_line(line).unwrap().unwrap() {
            RawMessage::SystemInit(init) => {
                assert_eq!(init.session_id.as_deref(), Some("sess-1"));
                assert_eq!(init.model.as_deref(), Some("claude-opus-4-8"));
                assert_eq!(init.tools, vec!["Bash", "Edit"]);
            }
            other => panic!("expected SystemInit, got {other:?}"),
        }
    }

    #[test]
    fn parses_assistant_text_and_tool_use() {
        let line = r#"{"type":"assistant","session_id":"s","message":{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"ls"}}]}}"#;
        match parse_line(line).unwrap().unwrap() {
            RawMessage::Assistant(env) => {
                assert_eq!(message_text(&env.message.content), "hi");
                let blocks = match env.message.content {
                    MessageContent::Blocks(b) => b,
                    _ => panic!("expected blocks"),
                };
                let has_tool = blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "Bash"));
                assert!(has_tool);
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn parses_string_content() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":"plain"}}"#;
        match parse_line(line).unwrap().unwrap() {
            RawMessage::Assistant(env) => {
                assert_eq!(message_text(&env.message.content), "plain")
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn parses_result_with_usage() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s","total_cost_usd":0.01,"num_turns":3,"usage":{"input_tokens":10,"output_tokens":20}}"#;
        match parse_line(line).unwrap().unwrap() {
            RawMessage::Result(r) => {
                assert!(!r.is_error);
                assert_eq!(r.result.as_deref(), Some("done"));
                assert_eq!(r.total_cost_usd, Some(0.01));
                assert_eq!(r.usage.and_then(|u| u.output_tokens), Some(20));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn parses_permission_control_request() {
        let line = r#"{"type":"control_request","request_id":"req-9","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf /"}}}"#;
        match parse_line(line).unwrap().unwrap() {
            RawMessage::ControlRequest(req) => {
                assert_eq!(req.request_id, "req-9");
                assert_eq!(req.request.subtype.as_deref(), Some("can_use_tool"));
                assert_eq!(req.request.tool_name.as_deref(), Some("Bash"));
            }
            other => panic!("expected ControlRequest, got {other:?}"),
        }
    }

    #[test]
    fn unknown_type_is_preserved() {
        let line = r#"{"type":"brand_new_thing","payload":42}"#;
        match parse_line(line).unwrap().unwrap() {
            RawMessage::Other(v) => assert_eq!(v.get("payload").and_then(Value::as_u64), Some(42)),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_error_not_panic() {
        assert!(parse_line("{not json").is_err());
    }
}
