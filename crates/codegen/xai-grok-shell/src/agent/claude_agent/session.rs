//! Subprocess lifecycle for the Claude Agent SDK harness.
//!
//! A [`ClaudeAgentSession`] owns a running `claude` process spoken to over
//! `stream-json`: user turns and control responses go in on stdin, harness
//! events come out on stdout. The turn-execution mode above the sampler drives
//! it — feed a user message, consume [`HarnessEvent`]s until [`HarnessEvent::TurnResult`],
//! answering any [`HarnessEvent::PermissionRequest`] via [`ClaudeAgentSession::respond_permission`].
//!
//! The pure builders ([`build_argv`], [`user_message_json`], [`permission_response_json`])
//! and the generic [`pump_events`] loop are unit-tested without spawning a real
//! process; the live subscription round-trip is verified by the user (no
//! `claude` login in CI).

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use super::{HarnessEvent, PermissionDecision, login, project, protocol};

/// Parameters for launching one harness session.
#[derive(Debug, Clone, Default)]
pub struct HarnessCommand {
    /// Model id passed to `--model` (e.g. `claude-opus-4-8`). `None` lets the
    /// harness pick its default.
    pub model: Option<String>,
    /// Prior `session_id` to continue via `--resume` for multi-turn continuity.
    pub resume: Option<String>,
    /// Working directory (`--add-dir` / process cwd).
    pub cwd: Option<String>,
    /// Permission mode forwarded to the harness (`default`, `acceptEdits`,
    /// `plan`, `bypassPermissions`). Atlas drives interactive prompts via the
    /// control protocol, so `default` is the norm.
    pub permission_mode: Option<String>,
    /// Stream partial assistant deltas (`--include-partial-messages`).
    pub partial_messages: bool,
    /// Escape hatch for additional flags without a code change.
    pub extra_args: Vec<String>,
}

/// Build the argv for `claude` in bidirectional stream-json mode. Pure so the
/// exact flags are pinned by tests rather than discovered at runtime.
pub fn build_argv(cmd: &HarnessCommand) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--print".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        // Required by the harness for stream-json output to include the
        // system/init and per-turn framing Atlas projects.
        "--verbose".into(),
    ];
    if cmd.partial_messages {
        args.push("--include-partial-messages".into());
    }
    if let Some(model) = &cmd.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    if let Some(resume) = &cmd.resume {
        args.push("--resume".into());
        args.push(resume.clone());
    }
    if let Some(mode) = &cmd.permission_mode {
        args.push("--permission-mode".into());
        args.push(mode.clone());
    }
    if let Some(cwd) = &cmd.cwd {
        args.push("--add-dir".into());
        args.push(cwd.clone());
    }
    args.extend(cmd.extra_args.iter().cloned());
    args
}

/// A `user` stream-json line carrying a plain-text turn.
pub fn user_message_json(text: &str) -> Value {
    json!({
        "type": "user",
        "message": { "role": "user", "content": text },
    })
}

/// A `control_response` answering a `can_use_tool` request. `updatedInput`
/// echoes the tool input unchanged on allow (the harness expects it back);
/// deny carries an optional message the harness surfaces to the model.
pub fn permission_response_json(
    request_id: &str,
    decision: &PermissionDecision,
    tool_input: &Value,
) -> Value {
    let response = match decision {
        PermissionDecision::Allow => json!({
            "behavior": "allow",
            "updatedInput": tool_input,
        }),
        PermissionDecision::Deny { reason } => json!({
            "behavior": "deny",
            "message": reason.clone().unwrap_or_else(|| "Denied by user".to_owned()),
        }),
    };
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        },
    })
}

/// An `interrupt` control request to stop the current turn.
pub fn interrupt_json() -> Value {
    json!({ "type": "control_request", "request": { "subtype": "interrupt" } })
}

/// A running harness session.
pub struct ClaudeAgentSession {
    child: Child,
    stdin: ChildStdin,
    events: mpsc::Receiver<HarnessEvent>,
}

impl ClaudeAgentSession {
    /// Spawn `claude` for a turn and start pumping its stdout into
    /// [`HarnessEvent`]s. Returns an error if the binary is missing (callers
    /// should have checked [`login::detect`] first for a friendlier message).
    pub fn spawn(cmd: &HarnessCommand) -> std::io::Result<Self> {
        let mut command = tokio::process::Command::new(login::binary_name());
        command
            .args(build_argv(cmd))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(cwd) = &cmd.cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");

        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            let _ = pump_events(BufReader::new(stdout), tx).await;
        });

        Ok(Self {
            child,
            stdin,
            events: rx,
        })
    }

    /// Next harness event, or `None` when the process has closed its stream.
    pub async fn next_event(&mut self) -> Option<HarnessEvent> {
        self.events.recv().await
    }

    /// Send a user turn into the running harness.
    pub async fn send_user_message(&mut self, text: &str) -> std::io::Result<()> {
        write_line(&mut self.stdin, &user_message_json(text)).await
    }

    /// Answer a pending [`super::PermissionRequest`].
    pub async fn respond_permission(
        &mut self,
        request_id: &str,
        decision: &PermissionDecision,
        tool_input: &Value,
    ) -> std::io::Result<()> {
        write_line(
            &mut self.stdin,
            &permission_response_json(request_id, decision, tool_input),
        )
        .await
    }

    /// Ask the harness to interrupt the current turn.
    pub async fn interrupt(&mut self) -> std::io::Result<()> {
        write_line(&mut self.stdin, &interrupt_json()).await
    }

    /// Best-effort terminate the child (also happens on drop via kill_on_drop).
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Serialize a JSON value as a single stdin line.
async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

/// Drive a line-delimited reader into projected [`HarnessEvent`]s. Generic over
/// the reader so tests can feed canned `stream-json` bytes without a process. A
/// malformed line is logged and skipped — one bad frame must not end the turn.
pub async fn pump_events<R>(reader: R, tx: mpsc::Sender<HarnessEvent>) -> std::io::Result<()>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        match protocol::parse_line(&line) {
            Ok(Some(raw)) => {
                for event in project(raw) {
                    if tx.send(event).await.is_err() {
                        return Ok(()); // receiver dropped; stop pumping.
                    }
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(target: "claude_agent", %err, "skipping malformed stream-json line");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_defaults_are_bidirectional_stream_json() {
        let argv = build_argv(&HarnessCommand::default());
        assert!(
            argv.windows(2)
                .any(|w| w == ["--input-format", "stream-json"])
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
        assert!(argv.contains(&"--print".to_owned()));
        assert!(argv.contains(&"--verbose".to_owned()));
    }

    #[test]
    fn argv_includes_model_and_resume() {
        let argv = build_argv(&HarnessCommand {
            model: Some("claude-opus-4-8".into()),
            resume: Some("sess-7".into()),
            ..Default::default()
        });
        assert!(argv.windows(2).any(|w| w == ["--model", "claude-opus-4-8"]));
        assert!(argv.windows(2).any(|w| w == ["--resume", "sess-7"]));
    }

    #[test]
    fn user_message_shape() {
        let v = user_message_json("hello");
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "hello");
    }

    #[test]
    fn allow_echoes_updated_input() {
        let input = json!({"command": "ls"});
        let v = permission_response_json("r1", &PermissionDecision::Allow, &input);
        assert_eq!(v["response"]["subtype"], "success");
        assert_eq!(v["response"]["request_id"], "r1");
        assert_eq!(v["response"]["response"]["behavior"], "allow");
        assert_eq!(v["response"]["response"]["updatedInput"], input);
    }

    #[test]
    fn deny_carries_reason() {
        let v = permission_response_json(
            "r2",
            &PermissionDecision::Deny {
                reason: Some("nope".into()),
            },
            &Value::Null,
        );
        assert_eq!(v["response"]["response"]["behavior"], "deny");
        assert_eq!(v["response"]["response"]["message"], "nope");
    }

    #[tokio::test]
    async fn pump_projects_a_canned_transcript() {
        // A stubbed `claude` transcript: init → assistant text + tool_use →
        // tool result → result. Proves the reader→project→channel wiring end to
        // end without spawning a process.
        let transcript = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-opus-4-8","tools":["Bash"]}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"listing"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"a.txt","is_error":false}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s1"}"#,
            "\n",
            "{ this line is malformed and must be skipped\n",
        );

        let (tx, mut rx) = mpsc::channel(64);
        let reader = BufReader::new(transcript.as_bytes());
        pump_events(reader, tx).await.unwrap();

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        assert!(matches!(events[0], HarnessEvent::SessionStarted { .. }));
        assert!(matches!(&events[1], HarnessEvent::AssistantText { text } if text == "listing"));
        assert!(matches!(&events[2], HarnessEvent::ToolUse { name, .. } if name == "Bash"));
        assert!(
            matches!(&events[3], HarnessEvent::ToolResult { content, .. } if content == "a.txt")
        );
        assert!(matches!(
            events.last().unwrap(),
            HarnessEvent::TurnResult {
                is_error: false,
                ..
            }
        ));
    }
}
