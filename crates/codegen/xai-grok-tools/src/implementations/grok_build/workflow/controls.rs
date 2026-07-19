use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    DEFAULT_CONCURRENCY, DEFAULT_RETENTION_DAYS, DEFAULT_TIMEOUT_SECONDS, MAX_ARGS_BYTES,
    MAX_CONCURRENCY, MAX_DIAGNOSTIC_BYTES, MAX_RESULT_BYTES, MAX_SCRIPT_BYTES,
    PRODUCTION_MAX_AGENTS, WORKFLOW_ACTION_TOOL_NAME, WORKFLOW_PREVIEW_TOOL_NAME, WorkflowInput,
    WorkflowMetadata, WorkflowTool, resolve_workflow_source, supervisor, transform_workflow_script,
    workflow_approval_hash,
};
use crate::implementations::grok_build::task::backend::SubagentBackendResource;
use crate::implementations::grok_build::task::types::SubagentCancelOutcome;
use crate::types::output::ToolOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::{Cwd, SessionFolder};
use crate::types::tool::{ToolKind, ToolNamespace};

const PREVIEW_DESCRIPTION: &str = r#"Validate and preview a dynamic workflow before execution.

This resolves an inline or saved workflow, validates its deterministic source form and limits, and returns an approval hash. The exact unchanged hash must be passed to `workflow`."#;

const ACTION_DESCRIPTION: &str = r#"Inspect and control dynamic workflow runs owned by this Atlas session.

Actions:
- `list`: list active and retained runs.
- `inspect`: return one run's durable lifecycle snapshot.
- `worker_details`: return the bounded tail of its worker journal.
- `pause`: stop active workers while preserving cached results for resume.
- `resume`: restart an interrupted or paused run, reusing completed labelled workers.
- `cancel_worker`: cancel one active worker that belongs to the run.
- `cancel`: stop active workers and mark the run cancelled."#;

#[derive(Debug, Default)]
pub struct WorkflowPreviewTool;

impl crate::types::tool_metadata::ToolMetadata for WorkflowPreviewTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        PREVIEW_DESCRIPTION
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::Value(ToolRequirement::tool::<WorkflowTool>())
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

impl xai_tool_runtime::Tool for WorkflowPreviewTool {
    type Args = WorkflowInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(WORKFLOW_PREVIEW_TOOL_NAME)
            .expect("valid workflow preview tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(WORKFLOW_PREVIEW_TOOL_NAME, PREVIEW_DESCRIPTION)
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        mut input: WorkflowInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let cwd = resources
            .lock()
            .await
            .get::<Cwd>()
            .map(|cwd| cwd.0.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        let source_path = resolve_workflow_source(&mut input, &cwd)
            .map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        validate_preview(&input).map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        let approval_hash = workflow_approval_hash(&input)
            .map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        let preview = json!({
            "approval_hash": approval_hash,
            "script_hash": blake3::hash(input.script.as_bytes()).to_hex().to_string(),
            "source": source_path,
            "saved_workflow": input.saved_workflow,
            "script_bytes": input.script.len(),
            "args_bytes": serde_json::to_vec(&input.args).map_or(0, |bytes| bytes.len()),
            "max_concurrency": input.max_concurrency.unwrap_or(DEFAULT_CONCURRENCY),
            "max_agents": input.max_agents.unwrap_or(super::MAX_AGENTS),
            "max_tokens": input.max_tokens,
            "timeout_seconds": input.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
            "retention_days": input.retention_days.unwrap_or(DEFAULT_RETENTION_DAYS),
            "run_in_background": input.run_in_background,
            "requests_write_workers": input.script.contains("mode: \"write\"")
                || input.script.contains("mode: 'write'"),
        });
        let text = serde_json::to_string_pretty(&preview).map_err(|error| {
            xai_tool_runtime::ToolError::custom("workflow_preview", error.to_string())
        })?;
        Ok(ToolOutput::Text(text.into()))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAction {
    List,
    Inspect,
    WorkerDetails,
    Pause,
    Resume,
    CancelWorker,
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowActionInput {
    pub action: WorkflowAction,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub worker_id: Option<String>,
    /// Resume in the background by default.
    #[serde(default = "default_true")]
    pub run_in_background: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default)]
pub struct WorkflowActionTool;

impl crate::types::tool_metadata::ToolMetadata for WorkflowActionTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        ACTION_DESCRIPTION
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::Value(ToolRequirement::tool::<WorkflowTool>())
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

impl xai_tool_runtime::Tool for WorkflowActionTool {
    type Args = WorkflowActionInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(WORKFLOW_ACTION_TOOL_NAME)
            .expect("valid workflow action tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(WORKFLOW_ACTION_TOOL_NAME, ACTION_DESCRIPTION)
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: WorkflowActionInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let (handle, session_folder, backend) = {
            let resources = resources.lock().await;
            let handle = resources
                .get::<supervisor::WorkflowSupervisorHandle>()
                .cloned()
                .ok_or_else(|| {
                    xai_tool_runtime::ToolError::custom(
                        "workflow_supervisor",
                        "workflow supervisor is unavailable",
                    )
                })?;
            let session_folder = resources
                .get::<SessionFolder>()
                .map(|folder| folder.0.clone())
                .ok_or_else(|| {
                    xai_tool_runtime::ToolError::custom(
                        "workflow_supervisor",
                        "session folder is unavailable",
                    )
                })?;
            let backend = resources.get::<SubagentBackendResource>().cloned();
            (handle, session_folder, backend)
        };

        let output = match input.action {
            WorkflowAction::List => {
                serde_json::to_value(supervisor::list(&handle).await.map_err(supervisor_error)?)
            }
            WorkflowAction::Inspect => {
                let run_id = required_run_id(input.run_id)?;
                serde_json::to_value(
                    supervisor::get(&handle, run_id.clone())
                        .await
                        .map_err(supervisor_error)?
                        .ok_or_else(|| {
                            xai_tool_runtime::ToolError::invalid_arguments(format!(
                                "workflow run '{run_id}' was not found"
                            ))
                        })?,
                )
            }
            WorkflowAction::WorkerDetails => {
                let run_id = required_run_id(input.run_id)?;
                let run_dir = supervisor::run_dir(&session_folder, &run_id);
                Ok(read_journal_tail(&run_dir.join("journal.jsonl"))?)
            }
            WorkflowAction::Pause | WorkflowAction::Cancel => {
                let run_id = required_run_id(input.run_id)?;
                let snapshot = supervisor::stop(
                        &handle,
                        run_id.clone(),
                        matches!(input.action, WorkflowAction::Pause),
                    )
                    .await
                    .map_err(supervisor_error)?;
                supervisor::wait_until_inactive(&handle, &run_id)
                    .await
                    .map_err(supervisor_error)?;
                serde_json::to_value(snapshot)
            }
            WorkflowAction::Resume => {
                let run_id = required_run_id(input.run_id)?;
                let snapshot = supervisor::get(&handle, run_id.clone())
                    .await
                    .map_err(supervisor_error)?
                    .ok_or_else(|| {
                        xai_tool_runtime::ToolError::invalid_arguments(format!(
                            "workflow run '{run_id}' was not found"
                        ))
                    })?;
                if !matches!(
                    snapshot.status,
                    supervisor::WorkflowRunStatus::Paused
                        | supervisor::WorkflowRunStatus::Interrupted
                        | supervisor::WorkflowRunStatus::Failed
                        | supervisor::WorkflowRunStatus::Partial
                ) {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "workflow run '{run_id}' cannot be resumed from {:?}",
                        snapshot.status
                    )));
                }
                return resume_run(ctx, session_folder, run_id, input.run_in_background).await;
            }
            WorkflowAction::CancelWorker => {
                let run_id = required_run_id(input.run_id)?;
                let worker_id = input.worker_id.ok_or_else(|| {
                    xai_tool_runtime::ToolError::invalid_arguments(
                        "cancel_worker requires worker_id",
                    )
                })?;
                if !worker_id.starts_with(&format!("workflow-{run_id}-")) {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(
                        "worker_id does not belong to the requested workflow run",
                    ));
                }
                let backend = backend.ok_or_else(|| {
                    xai_tool_runtime::ToolError::custom(
                        "workflow_worker",
                        "subagent backend is unavailable",
                    )
                })?;
                let outcome = backend.0.cancel(&worker_id).await;
                Ok(match outcome {
                    SubagentCancelOutcome::Cancelled => {
                        json!({"worker_id": worker_id, "status": "cancelled"})
                    }
                    SubagentCancelOutcome::AlreadyFinished { status } => {
                        json!({"worker_id": worker_id, "status": "already_finished", "worker_status": status})
                    }
                    SubagentCancelOutcome::NotFound => {
                        json!({"worker_id": worker_id, "status": "not_found"})
                    }
                })
            }
        }
        .map_err(|error| {
            xai_tool_runtime::ToolError::custom("workflow_action", error.to_string())
        })?;
        let text = serde_json::to_string_pretty(&output).map_err(|error| {
            xai_tool_runtime::ToolError::custom("workflow_action", error.to_string())
        })?;
        Ok(ToolOutput::Text(text.into()))
    }
}

fn validate_preview(input: &WorkflowInput) -> Result<(), String> {
    if input.script.len() > MAX_SCRIPT_BYTES {
        return Err(format!(
            "workflow script exceeds the {MAX_SCRIPT_BYTES}-byte limit"
        ));
    }
    transform_workflow_script(&input.script)?;
    let args_bytes = serde_json::to_vec(&input.args)
        .map_err(|error| format!("workflow args are not valid JSON: {error}"))?;
    if args_bytes.len() > MAX_ARGS_BYTES {
        return Err(format!(
            "workflow args exceed the {MAX_ARGS_BYTES}-byte limit"
        ));
    }
    let concurrency = input.max_concurrency.unwrap_or(DEFAULT_CONCURRENCY);
    if !(1..=MAX_CONCURRENCY).contains(&concurrency) {
        return Err(format!(
            "max_concurrency must be between 1 and {MAX_CONCURRENCY}"
        ));
    }
    let max_agents = input.max_agents.unwrap_or(super::MAX_AGENTS);
    if !(1..=PRODUCTION_MAX_AGENTS).contains(&max_agents) {
        return Err(format!(
            "max_agents must be between 1 and {PRODUCTION_MAX_AGENTS}"
        ));
    }
    if input.max_tokens == Some(0) {
        return Err("max_tokens must be greater than zero".to_string());
    }
    Ok(())
}

fn required_run_id(run_id: Option<String>) -> Result<String, xai_tool_runtime::ToolError> {
    let run_id = run_id.ok_or_else(|| {
        xai_tool_runtime::ToolError::invalid_arguments("this action requires run_id")
    })?;
    supervisor::validate_run_id(&run_id).map_err(xai_tool_runtime::ToolError::invalid_arguments)
}

fn supervisor_error(error: String) -> xai_tool_runtime::ToolError {
    xai_tool_runtime::ToolError::custom("workflow_supervisor", error)
}

pub fn read_journal_tail(path: &Path) -> Result<Value, xai_tool_runtime::ToolError> {
    let metadata = fs::metadata(path).map_err(|error| {
        xai_tool_runtime::ToolError::custom(
            "workflow_journal",
            format!("failed to inspect {}: {error}", path.display()),
        )
    })?;
    if metadata.len() > (MAX_RESULT_BYTES + MAX_DIAGNOSTIC_BYTES) as u64 {
        return Err(xai_tool_runtime::ToolError::custom(
            "workflow_journal",
            "workflow journal is too large to inspect directly",
        ));
    }
    let content = fs::read_to_string(path).map_err(|error| {
        xai_tool_runtime::ToolError::custom(
            "workflow_journal",
            format!("failed to read {}: {error}", path.display()),
        )
    })?;
    let events = content
        .lines()
        .rev()
        .take(500)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    Ok(json!({ "events_newest_first": events }))
}

pub async fn resume_run(
    ctx: xai_tool_runtime::ToolCallContext,
    session_folder: PathBuf,
    run_id: String,
    run_in_background: bool,
) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
    let run_dir = supervisor::run_dir(&session_folder, &run_id);
    let metadata: WorkflowMetadata =
        super::read_json_bounded(&run_dir.join("metadata.json"), 64 * 1024)
            .map_err(supervisor_error)?;
    let script = read_string_bounded(&run_dir.join("script.js"), MAX_SCRIPT_BYTES)?;
    let args: Value = super::read_json_bounded(&run_dir.join("args.json"), MAX_ARGS_BYTES)
        .map_err(supervisor_error)?;
    let mut workflow_input = WorkflowInput {
        script,
        saved_workflow: None,
        args,
        resume_from_run_id: Some(run_id),
        max_concurrency: Some(metadata.max_concurrency),
        timeout_seconds: metadata.timeout_seconds,
        max_agents: Some(metadata.max_agents),
        max_tokens: metadata.max_tokens,
        retention_days: metadata.retention_days,
        run_in_background,
        approval_hash: None,
    };
    workflow_input.approval_hash = Some(
        workflow_approval_hash(&workflow_input)
            .map_err(xai_tool_runtime::ToolError::invalid_arguments)?,
    );
    xai_tool_runtime::Tool::run(&WorkflowTool, ctx, workflow_input).await
}

fn read_string_bounded(path: &Path, limit: usize) -> Result<String, xai_tool_runtime::ToolError> {
    let metadata = fs::metadata(path).map_err(|error| {
        xai_tool_runtime::ToolError::custom(
            "workflow_resume",
            format!("failed to inspect {}: {error}", path.display()),
        )
    })?;
    if metadata.len() > limit as u64 {
        return Err(xai_tool_runtime::ToolError::custom(
            "workflow_resume",
            format!("{} exceeds its size limit", path.display()),
        ));
    }
    fs::read_to_string(path).map_err(|error| {
        xai_tool_runtime::ToolError::custom(
            "workflow_resume",
            format!("failed to read {}: {error}", path.display()),
        )
    })
}
