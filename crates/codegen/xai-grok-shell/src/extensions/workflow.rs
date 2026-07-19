//! Pager-facing Dynamic Workflow inspection and control.
//!
//! The model tools and the TUI share the same session-owned supervisor. These
//! ACP methods are intentionally narrow: the client can only address the
//! supervisor belonging to the supplied Atlas session.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};

use crate::agent::mvp_agent::MvpAgent;
use crate::extensions::{ExtResult, to_raw_response};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowControlAction {
    List,
    Inspect,
    Workers,
    Pause,
    Resume,
    CancelWorker,
    Cancel,
    SetUltracode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowControlRequest {
    pub session_id: String,
    pub action: WorkflowControlAction,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    if args.method.as_ref() != "x.ai/workflow/control" {
        return Err(acp::Error::method_not_found());
    }
    let request: WorkflowControlRequest = serde_json::from_str(args.params.get())
        .map_err(|error| acp::Error::invalid_params().data(error.to_string()))?;
    let session_id = acp::SessionId::new(request.session_id.clone());
    let handle = agent
        .get_session_handle(&session_id)
        .ok_or_else(|| acp::Error::invalid_params().data("session not found"))?;
    let response = handle
        .workflow_control(request)
        .await
        .map_err(|error| acp::Error::internal_error().data(error))?;
    to_raw_response(&response)
}
