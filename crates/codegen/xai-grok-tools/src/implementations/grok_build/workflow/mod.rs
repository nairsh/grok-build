//! Experimental dynamic-workflow proof of concept.
//!
//! The model supplies a small JavaScript orchestration program. QuickJS runs
//! that program without filesystem, process, network, or module-loader access,
//! while the native `agent()` binding delegates read-only work to the existing
//! subagent coordinator. Intermediate values stay inside the workflow runtime;
//! only the final JSON value is returned to the parent model.

pub mod controls;
pub mod supervisor;

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fs2::FileExt as _;
use rquickjs::prelude::{Async, CatchResultExt, Func};
use rquickjs::{AsyncContext, AsyncRuntime, Promise};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use xai_tool_types::{SubagentCapabilityMode, SubagentIsolationMode};

use super::task::backend::SubagentBackendResource;
use super::task::types::{
    CurrentPromptIdResource, ModelOverrideProvenance, SessionIdResource, SubagentCancelOutcome,
    SubagentRequest, SubagentRuntimeOverrides, TaskModelValidator,
};
use crate::types::output::ToolOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::{Cwd, SessionFolder};
use crate::types::tool::{ToolKind, ToolNamespace};

pub const WORKFLOW_TOOL_NAME: &str = "workflow";
pub const WORKFLOW_PREVIEW_TOOL_NAME: &str = "workflow_preview";
pub const WORKFLOW_ACTION_TOOL_NAME: &str = "workflow_action";
pub const EXPERIMENTAL_WORKFLOWS_ENV: &str = "ATLAS_EXPERIMENTAL_WORKFLOWS";
pub const ULTRACODE_ENV: &str = "ATLAS_ULTRACODE";

const MAX_SCRIPT_BYTES: usize = 128 * 1024;
const MAX_ARGS_BYTES: usize = 64 * 1024;
const MAX_AGENT_OPTIONS_BYTES: usize = 64 * 1024;
const MAX_RESULT_BYTES: usize = 512 * 1024;
const MAX_CACHE_RECORD_BYTES: usize = MAX_RESULT_BYTES + MAX_AGENT_OPTIONS_BYTES + 4 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const MAX_CONCURRENCY: usize = 16;
const MAX_AGENTS: usize = 16;
const PRODUCTION_MAX_AGENTS: usize = 256;
const DEFAULT_RETENTION_DAYS: u64 = 7;
const MAX_RETENTION_DAYS: u64 = 30;
const MAX_PHASE_EVENTS: usize = 128;
const DEFAULT_CONCURRENCY: usize = 4;
const DEFAULT_TIMEOUT_SECONDS: u64 = 30 * 60;
const MAX_TIMEOUT_SECONDS: u64 = 30 * 60;
const JS_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const JS_STACK_LIMIT: usize = 1024 * 1024;

const WORKFLOW_DESCRIPTION: &str = r#"Run a supervised multi-agent workflow from deterministic JavaScript.

Use this for substantive work that benefits from independent discovery, implementation, review, or verification. Call `workflow_preview` first and pass its exact approval hash. Use `workflow_action` to inspect, pause, resume, or cancel retained runs. The workflow coordinator cannot directly read files, run commands, access the network, or mutate the workspace; it can only schedule strictly scoped Atlas child sessions.

The script must declare literal metadata and return one JSON-serializable value:

```js
export const meta = {
  name: "audit-routes",
  description: "Inspect routes and verify findings"
};

phase("Inspect");
const results = await pipeline(
  args.files,
  (file) => agent(`Inspect ${file}`, {
    label: `inspect:${file}`,
    schema: {
      type: "object",
      required: ["file", "finding"],
      properties: {
        file: { type: "string" },
        finding: { type: "string" }
      }
    }
  }),
  (inspection, file) => agent(
    `Verify ${file}: ${JSON.stringify(inspection)}`,
    { label: `verify:${file}` }
  )
);
return results.filter(Boolean);
```

Available globals:
- `args`: immutable JSON supplied in the tool call.
- `agent(prompt, options)`: returns text, or a validated JSON value when `options.schema` is present. Options: `label`, `phase`, `model`, `effort`, `schema`, `mode`, `isolation`. Give every replayable call a unique stable label. Workers default to a strict read-only leaf. A `mode: "write"` worker is allowed only in a fail-closed isolated Git worktree.
- `parallel([() => agent(...), ...])`: starts independent branches and preserves their input order.
- `pipeline(items, ...stages)`: processes every item through its stages without global barriers. A stage receives `(currentValue, originalItem, itemIndex)`.
- `phase(name)`: records the current progress phase.

Worker failures become `null` so unrelated items can continue. Runtime or budget errors fail the workflow. Runs default to 4 concurrent and 16 total workers and can opt into as many as 256 total workers. To retry an interrupted run without repeating completed labelled workers, use `workflow_action` with `action: "resume"`."#;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowInput {
    /// JavaScript workflow source. Must contain `export const meta = { ... }`
    /// and return a JSON-serializable value.
    #[serde(default)]
    pub script: String,
    /// Saved workflow name. Project `.atlas/workflows` takes precedence over
    /// the user's `~/.atlas/workflows` directory.
    #[serde(default)]
    pub saved_workflow: Option<String>,
    /// Immutable JSON exposed to the script as `args`.
    #[serde(default = "empty_object")]
    pub args: Value,
    /// Resume a run from this session. The script and args must match exactly.
    #[serde(default)]
    pub resume_from_run_id: Option<String>,
    /// Maximum simultaneously active workers (1-4 for the proof of concept).
    #[serde(default)]
    pub max_concurrency: Option<usize>,
    /// JavaScript execution deadline. Worker API waits are not CPU time, but
    /// the deadline still prevents unbounded script loops.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Maximum total worker calls over the lifetime of this run.
    #[serde(default)]
    pub max_agents: Option<usize>,
    /// Optional aggregate worker-token budget.
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Number of days to retain the run journal and cached worker results.
    #[serde(default)]
    pub retention_days: Option<u64>,
    /// Return after the deterministic supervisor accepts the run.
    #[serde(default)]
    pub run_in_background: bool,
    /// Exact approval hash returned by `workflow_preview`.
    #[serde(default)]
    pub approval_hash: Option<String>,
}

fn empty_object() -> Value {
    json!({})
}

#[derive(Debug, Default)]
pub struct WorkflowTool;

impl crate::types::tool_metadata::ToolMetadata for WorkflowTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        WORKFLOW_DESCRIPTION
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::Value(ToolRequirement::tool::<super::task::TaskTool>())
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

impl xai_tool_runtime::Tool for WorkflowTool {
    type Args = WorkflowInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(WORKFLOW_TOOL_NAME).expect("valid workflow tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(WORKFLOW_TOOL_NAME, WORKFLOW_DESCRIPTION)
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        // Although v0 workers are read-only, launching autonomous agents is an
        // important action and must not silently inherit read-tool permission.
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.workflow", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        mut input: WorkflowInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let run_in_background = input.run_in_background;
        let cancellation = if run_in_background {
            CancellationToken::new()
        } else {
            ctx.get::<xai_tool_runtime::Cancellation>()
                .map(|value| value.0.clone())
                .unwrap_or_default()
        };
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let (
            backend,
            model_validator,
            session_id,
            parent_prompt_id,
            cwd,
            session_folder,
            supervisor_handle,
        ) = {
            let res = resources.lock().await;
            (
                res.get::<SubagentBackendResource>()
                    .cloned()
                    .ok_or_else(|| {
                        xai_tool_runtime::ToolError::custom(
                            "missing_resource",
                            "Workflow support requires the subagent coordinator",
                        )
                    })?,
                res.get::<TaskModelValidator>().cloned(),
                res.get::<SessionIdResource>()
                    .map(|v| v.0.clone())
                    .unwrap_or_default(),
                res.get::<CurrentPromptIdResource>()
                    .map(|v| v.0.clone())
                    .filter(|v| !v.is_empty()),
                res.get::<Cwd>()
                    .map(|v| v.0.clone())
                    .unwrap_or_else(|| PathBuf::from(".")),
                res.get::<SessionFolder>()
                    .map(|v| v.0.clone())
                    .ok_or_else(|| {
                        xai_tool_runtime::ToolError::custom(
                            "missing_resource",
                            "Workflow support requires a session folder",
                        )
                    })?,
                res.get::<supervisor::WorkflowSupervisorHandle>().cloned(),
            )
        };

        resolve_workflow_source(&mut input, &cwd)
            .map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        if supervisor_handle.is_some() {
            let expected = workflow_approval_hash(&input)
                .map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
            if input.approval_hash.as_deref() != Some(expected.as_str()) {
                return Err(xai_tool_runtime::ToolError::custom(
                    "workflow_approval_required",
                    format!(
                        "workflow source or run settings have not been approved; call \
                         `{WORKFLOW_PREVIEW_TOOL_NAME}` and pass its approval_hash unchanged"
                    ),
                ));
            }
        }
        let prepared = PreparedRun::new(
            input,
            backend,
            model_validator,
            session_id,
            parent_prompt_id,
            cwd,
            session_folder,
            cancellation,
        )
        .map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        if let Some(supervisor_handle) = supervisor_handle {
            let started = supervisor::start(&supervisor_handle, prepared)
                .await
                .map_err(|error| {
                    xai_tool_runtime::ToolError::custom("workflow_supervisor", error)
                })?;
            if run_in_background {
                let text = serde_json::to_string_pretty(&started.snapshot).map_err(|error| {
                    xai_tool_runtime::ToolError::custom("workflow_output", error.to_string())
                })?;
                return Ok(ToolOutput::Text(text.into()));
            }
            let terminal = supervisor::wait_for_terminal(started.updates)
                .await
                .map_err(|error| {
                    xai_tool_runtime::ToolError::custom("workflow_supervisor", error)
                })?;
            let summary: Value = read_json_bounded(
                &terminal.run_dir.join("result.json"),
                MAX_RESULT_BYTES + MAX_DIAGNOSTIC_BYTES,
            )
            .map_err(|error| xai_tool_runtime::ToolError::custom("workflow_execution", error))?;
            if matches!(
                terminal.status,
                supervisor::WorkflowRunStatus::Completed | supervisor::WorkflowRunStatus::Partial
            ) {
                let text = serde_json::to_string_pretty(&summary).map_err(|error| {
                    xai_tool_runtime::ToolError::custom("workflow_output", error.to_string())
                })?;
                return Ok(ToolOutput::Text(text.into()));
            }
            return Err(xai_tool_runtime::ToolError::custom(
                "workflow_execution",
                format!(
                    "{}\nworkflow_run_id: {}\nworkflow_dir: {}",
                    terminal
                        .error
                        .unwrap_or_else(|| format!("workflow ended as {:?}", terminal.status)),
                    terminal.run_id,
                    terminal.run_dir.display()
                ),
            ));
        }
        let state = prepared.state.clone();
        let _cancel_on_drop = CancelOnDrop(state.cancellation.clone());
        let script = prepared.script;
        let args_json = prepared.args_json;
        let timeout = prepared.timeout;

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name(format!("workflow-{}", short_id(&state.run_id)))
            .spawn({
                let state = state.clone();
                move || {
                    let execution = tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                        .map_err(|e| format!("failed to create workflow runtime: {e}"))
                        .and_then(|runtime| {
                            let local = tokio::task::LocalSet::new();
                            local.block_on(
                                &runtime,
                                execute_javascript(state.clone(), script, args_json, timeout),
                            )
                        });
                    let result = match execution {
                        Ok(value) => state.finish(value).inspect_err(|error| {
                            state.persist_failure(error);
                        }),
                        Err(error) => {
                            let error = bounded_diagnostic(error);
                            state.persist_failure(&error);
                            Err(error)
                        }
                    };
                    let _ = done_tx.send(result);
                }
            })
            .map_err(|e| {
                xai_tool_runtime::ToolError::custom(
                    "workflow_runtime",
                    format!("failed to start workflow runtime: {e}"),
                )
            })?;

        let execution = done_rx.await.map_err(|_| {
            xai_tool_runtime::ToolError::custom(
                "workflow_runtime",
                "workflow runtime exited without returning a result",
            )
        })?;

        match execution {
            Ok(summary) => {
                let text = serde_json::to_string_pretty(&summary).map_err(|e| {
                    xai_tool_runtime::ToolError::custom(
                        "workflow_output",
                        format!("failed to serialize workflow summary: {e}"),
                    )
                })?;
                Ok(ToolOutput::Text(text.into()))
            }
            Err(error) => Err(xai_tool_runtime::ToolError::custom(
                "workflow_execution",
                format!(
                    "{error}\nworkflow_run_id: {}\nworkflow_dir: {}",
                    state.run_id,
                    state.run_dir.display()
                ),
            )),
        }
    }
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(crate) struct PreparedRun {
    state: Arc<WorkflowState>,
    script: String,
    args_json: String,
    timeout: Duration,
}

impl PreparedRun {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: WorkflowInput,
        backend: SubagentBackendResource,
        model_validator: Option<TaskModelValidator>,
        session_id: String,
        parent_prompt_id: Option<String>,
        cwd: PathBuf,
        session_folder: PathBuf,
        cancellation: CancellationToken,
    ) -> Result<Self, String> {
        if input.script.len() > MAX_SCRIPT_BYTES {
            return Err(format!(
                "workflow script exceeds the {MAX_SCRIPT_BYTES}-byte proof-of-concept limit"
            ));
        }
        let transformed_script = transform_workflow_script(&input.script)?;
        let args_json = serde_json::to_string(&input.args)
            .map_err(|e| format!("workflow args are not valid JSON: {e}"))?;
        if args_json.len() > MAX_ARGS_BYTES {
            return Err(format!(
                "workflow args exceed the {MAX_ARGS_BYTES}-byte proof-of-concept limit"
            ));
        }
        let concurrency = input.max_concurrency.unwrap_or(DEFAULT_CONCURRENCY);
        if !(1..=MAX_CONCURRENCY).contains(&concurrency) {
            return Err(format!(
                "max_concurrency must be between 1 and {MAX_CONCURRENCY}"
            ));
        }
        let max_agents = input.max_agents.unwrap_or(MAX_AGENTS);
        if !(1..=PRODUCTION_MAX_AGENTS).contains(&max_agents) {
            return Err(format!(
                "max_agents must be between 1 and {PRODUCTION_MAX_AGENTS}"
            ));
        }
        if input.max_tokens == Some(0) {
            return Err("max_tokens must be greater than zero".to_string());
        }
        let retention_days = input.retention_days.unwrap_or(DEFAULT_RETENTION_DAYS);
        if !(1..=MAX_RETENTION_DAYS).contains(&retention_days) {
            return Err(format!(
                "retention_days must be between 1 and {MAX_RETENTION_DAYS}"
            ));
        }
        let timeout_seconds = input.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        if !(1..=MAX_TIMEOUT_SECONDS).contains(&timeout_seconds) {
            return Err(format!(
                "timeout_seconds must be between 1 and {MAX_TIMEOUT_SECONDS}"
            ));
        }

        let script_hash = stable_hash(input.script.as_bytes());
        let args_hash = stable_hash(args_json.as_bytes());
        let workflows_dir = session_folder.join("workflows");
        fs::create_dir_all(&workflows_dir)
            .map_err(|e| format!("failed to create workflow state directory: {e}"))?;

        let (run_id, run_dir, resumed, created_at) = match input.resume_from_run_id {
            Some(raw_id) => {
                let run_id = uuid::Uuid::parse_str(raw_id.trim())
                    .map_err(|_| "resume_from_run_id must be a workflow UUID".to_string())?
                    .to_string();
                let run_dir = workflows_dir.join(&run_id);
                if !run_dir.is_dir() {
                    return Err(format!(
                        "workflow run '{run_id}' was not found in this session"
                    ));
                }
                let metadata: WorkflowMetadata =
                    read_json_bounded(&run_dir.join("metadata.json"), 64 * 1024).map_err(|e| {
                        format!("failed to load workflow run '{run_id}' metadata: {e}")
                    })?;
                if metadata.script_hash != script_hash || metadata.args_hash != args_hash {
                    return Err(
                        "resume requires the exact same workflow script and args".to_string()
                    );
                }
                if metadata.max_concurrency != concurrency
                    || metadata.max_agents != max_agents
                    || metadata.max_tokens != input.max_tokens
                    || metadata.retention_days.unwrap_or(DEFAULT_RETENTION_DAYS) != retention_days
                    || metadata.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS)
                        != timeout_seconds
                {
                    return Err(
                        "resume requires the exact same concurrency, agent, token, and retention limits"
                            .to_string(),
                    );
                }
                (run_id, run_dir, true, metadata.created_at)
            }
            None => {
                let run_id = uuid::Uuid::now_v7().to_string();
                let run_dir = workflows_dir.join(&run_id);
                fs::create_dir_all(run_dir.join("results"))
                    .map_err(|e| format!("failed to create workflow run directory: {e}"))?;
                let created_at = chrono::Utc::now().to_rfc3339();
                let metadata = WorkflowMetadata {
                    version: 1,
                    run_id: run_id.clone(),
                    session_id: session_id.clone(),
                    script_hash: script_hash.clone(),
                    args_hash: args_hash.clone(),
                    max_concurrency: concurrency,
                    max_agents,
                    max_tokens: input.max_tokens,
                    retention_days: Some(retention_days),
                    timeout_seconds: Some(timeout_seconds),
                    created_at: created_at.clone(),
                };
                write_json_atomic(&run_dir.join("metadata.json"), &metadata)?;
                write_atomic(&run_dir.join("script.js"), input.script.as_bytes())?;
                write_atomic(&run_dir.join("args.json"), args_json.as_bytes())?;
                (run_id, run_dir, false, created_at)
            }
        };
        let run_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(run_dir.join("run.lock"))
            .map_err(|e| format!("failed to open workflow run lock: {e}"))?;
        run_lock.try_lock_exclusive().map_err(|error| {
            format!(
                "workflow run '{run_id}' is already active and cannot be resumed concurrently: {error}"
            )
        })?;

        let state = Arc::new(WorkflowState {
            run_id,
            attempt_id: uuid::Uuid::now_v7().to_string(),
            run_dir,
            _run_lock: run_lock,
            backend,
            model_validator,
            session_id,
            parent_prompt_id,
            cwd,
            semaphore: Arc::new(Semaphore::new(concurrency)),
            max_concurrency: concurrency,
            max_agents,
            max_tokens: input.max_tokens,
            script_hash,
            created_at,
            resumed,
            seen_labels: Mutex::new(HashSet::new()),
            journal_lock: Mutex::new(()),
            current_phase: Mutex::new(None),
            phases: Mutex::new(Vec::new()),
            active_ids: Mutex::new(HashSet::new()),
            invocations: AtomicUsize::new(0),
            settled_invocations: AtomicUsize::new(0),
            spawned: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            cache_hits: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            tokens_used: AtomicU64::new(0),
            started_at: Instant::now(),
            deadline: Instant::now() + Duration::from_secs(timeout_seconds),
            cancellation,
        });
        state.append_journal(&JournalEntry {
            event: if resumed { "resumed" } else { "started" },
            label: None,
            phase: None,
            subagent_id: None,
            input_hash: None,
            error: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
        });

        Ok(Self {
            state,
            script: transformed_script,
            args_json,
            timeout: Duration::from_secs(timeout_seconds),
        })
    }
}

pub(crate) struct WorkflowState {
    run_id: String,
    attempt_id: String,
    run_dir: PathBuf,
    _run_lock: File,
    backend: SubagentBackendResource,
    model_validator: Option<TaskModelValidator>,
    session_id: String,
    parent_prompt_id: Option<String>,
    cwd: PathBuf,
    semaphore: Arc<Semaphore>,
    max_concurrency: usize,
    max_agents: usize,
    max_tokens: Option<u64>,
    script_hash: String,
    created_at: String,
    resumed: bool,
    seen_labels: Mutex<HashSet<String>>,
    journal_lock: Mutex<()>,
    current_phase: Mutex<Option<String>>,
    phases: Mutex<Vec<String>>,
    active_ids: Mutex<HashSet<String>>,
    invocations: AtomicUsize,
    settled_invocations: AtomicUsize,
    spawned: AtomicUsize,
    completed: AtomicUsize,
    failed: AtomicUsize,
    cache_hits: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    tokens_used: AtomicU64,
    started_at: Instant,
    deadline: Instant,
    cancellation: CancellationToken,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentOptions {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    schema: Option<Value>,
    /// `read` (default) or `write`. Write workers are always worktree-isolated.
    #[serde(default)]
    mode: Option<String>,
    /// Optional `worktree` isolation. Required for write workers.
    #[serde(default)]
    isolation: Option<String>,
}

#[derive(Serialize)]
struct NativeAgentReply {
    fatal: bool,
    value: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl NativeAgentReply {
    fn value(value: Value) -> String {
        serde_json::to_string(&Self {
            fatal: false,
            value,
            error: None,
        })
        .expect("native workflow reply serializes")
    }

    fn worker_failure(error: impl Into<String>) -> String {
        serde_json::to_string(&Self {
            fatal: false,
            value: Value::Null,
            error: Some(error.into()),
        })
        .expect("native workflow reply serializes")
    }

    fn fatal(error: impl Into<String>) -> String {
        serde_json::to_string(&Self {
            fatal: true,
            value: Value::Null,
            error: Some(error.into()),
        })
        .expect("native workflow reply serializes")
    }
}

impl WorkflowState {
    fn admit_agent_call(
        &self,
        prompt: String,
        options_json: String,
    ) -> Result<(String, String, AgentOptions, usize), String> {
        if self.cancellation.is_cancelled() {
            return Err("workflow was cancelled".to_string());
        }
        if Instant::now() >= self.deadline {
            self.cancellation.cancel();
            return Err("workflow deadline exceeded".to_string());
        }
        if self
            .max_tokens
            .is_some_and(|limit| self.tokens_used.load(Ordering::Relaxed) >= limit)
        {
            self.cancellation.cancel();
            return Err("workflow token budget exhausted".to_string());
        }
        if prompt.trim().is_empty() {
            return Err("agent prompt must not be empty".to_string());
        }
        if prompt.len() > MAX_SCRIPT_BYTES {
            return Err("agent prompt exceeds the workflow size limit".to_string());
        }
        if options_json.len() > MAX_AGENT_OPTIONS_BYTES {
            return Err("agent options exceed the workflow size limit".to_string());
        }
        let options: AgentOptions = serde_json::from_str(&options_json)
            .map_err(|error| format!("invalid agent options: {error}"))?;
        let mode = options.mode.as_deref().unwrap_or("read");
        if !matches!(mode, "read" | "write") {
            return Err("agent options.mode must be `read` or `write`".to_string());
        }
        let isolation = options.isolation.as_deref().unwrap_or(if mode == "write" {
            "worktree"
        } else {
            "none"
        });
        if !matches!(isolation, "none" | "worktree") {
            return Err("agent options.isolation must be `none` or `worktree`".to_string());
        }
        if mode == "write" && isolation != "worktree" {
            return Err("workflow write workers require isolation: `worktree`".to_string());
        }
        if let Some(schema) = options.schema.as_ref() {
            validate_workflow_schema(schema)?;
            jsonschema::validator_for(schema)
                .map_err(|error| format!("invalid workflow agent schema: {error}"))?;
        }
        let invocation_index = self
            .reserve_invocation_slot()
            .ok_or_else(|| format!("workflow exceeded the {}-agent run limit", self.max_agents))?;
        Ok((prompt, options_json, options, invocation_index))
    }

    async fn execute_agent(
        self: Arc<Self>,
        prompt: String,
        options_json: String,
        options: AgentOptions,
        invocation_index: usize,
    ) -> String {
        let label = options
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_owned);
        if let Some(label) = &label {
            let mut seen = self.seen_labels.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.insert(label.clone()) {
                return NativeAgentReply::fatal(format!(
                    "workflow agent label '{label}' is duplicated"
                ));
            }
        }

        let input_hash =
            stable_hash(format!("{prompt}\n{options_json}\nworkflow-poc-v1").as_bytes());
        if self.resumed
            && let Some(label) = &label
            && let Some(value) = self.read_cached(label, &input_hash)
        {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
            self.append_journal(&JournalEntry {
                event: "cache_hit",
                label: Some(label.clone()),
                phase: options.phase.clone(),
                subagent_id: None,
                input_hash: Some(input_hash),
                error: None,
                timestamp: chrono::Utc::now().to_rfc3339(),
            });
            return NativeAgentReply::value(value);
        }

        // Use the semaphore to bound validation as well as execution. The
        // finite call budget was reserved synchronously by the native binding
        // before this future was created.
        let permit = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                return NativeAgentReply::fatal("workflow was cancelled");
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.deadline)) => {
                self.cancellation.cancel();
                return NativeAgentReply::fatal("workflow deadline exceeded");
            }
            permit = self.semaphore.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    return NativeAgentReply::fatal("workflow scheduler closed unexpectedly");
                }
            }
        };

        if let Some(model) = options.model.as_deref()
            && let Some(error) = self
                .model_validator
                .as_ref()
                .and_then(|validator| validator.error_for(model))
        {
            return NativeAgentReply::fatal(error);
        }

        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.spawned.fetch_add(1, Ordering::Relaxed);
        let subagent_id = format!(
            "workflow-{}-{}-{invocation_index}",
            self.run_id, self.attempt_id
        );
        self.active_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(subagent_id.clone());
        let phase = options.phase.clone().or_else(|| {
            self.current_phase
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        self.append_journal(&JournalEntry {
            event: "agent_started",
            label: label.clone(),
            phase: phase.clone(),
            subagent_id: Some(subagent_id.clone()),
            input_hash: Some(input_hash.clone()),
            error: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
        });

        let (result_tx, _) = tokio::sync::oneshot::channel();
        let write_worker = options.mode.as_deref() == Some("write");
        let worktree_isolation = write_worker || options.isolation.as_deref() == Some("worktree");
        let request = SubagentRequest {
            id: subagent_id.clone(),
            prompt,
            // The workflow runtime keeps and revalidates the complete JSON
            // Schema below. Native structured-output providers support a
            // narrower dialect, so send them a compatible projection rather
            // than letting validation-only keywords reject the whole request.
            json_schema: options.schema.as_ref().map(provider_compatible_schema),
            description: label
                .clone()
                .unwrap_or_else(|| format!("workflow worker {invocation_index}")),
            // The shell ignores name-based discovery when `strict_read_only`
            // is set and substitutes its unshadowable built-in leaf profile.
            subagent_type: "explore".to_string(),
            parent_session_id: self.session_id.clone(),
            parent_prompt_id: self.parent_prompt_id.clone(),
            resume_from: None,
            cwd: Some(self.cwd.to_string_lossy().into_owned()),
            runtime_overrides: SubagentRuntimeOverrides {
                model: options.model,
                model_override_provenance: ModelOverrideProvenance::Tool,
                reasoning_effort: options.effort,
                persona: None,
                capability_mode: Some(if write_worker {
                    SubagentCapabilityMode::ReadWrite
                } else {
                    SubagentCapabilityMode::ReadOnly
                }),
                isolation: Some(if worktree_isolation {
                    SubagentIsolationMode::Worktree
                } else {
                    SubagentIsolationMode::None
                }),
                harness_agent_type: None,
            },
            // This avoids the ordinary foreground auto-background deadline.
            // The workflow still awaits the backend result and owns the wait.
            run_in_background: true,
            surface_completion: false,
            fork_context: false,
            strict_read_only: !write_worker,
            strict_workflow_write: write_worker,
            result_tx,
        };
        let backend_result = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                if self.finish_active(&subagent_id) {
                    let _ = cancel_worker(&self.backend, &subagent_id).await;
                }
                drop(permit);
                self.append_journal(&JournalEntry {
                    event: "agent_cancelled",
                    label,
                    phase,
                    subagent_id: Some(subagent_id),
                    input_hash: Some(input_hash),
                    error: Some("workflow was cancelled".to_string()),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
                return NativeAgentReply::fatal("workflow was cancelled");
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.deadline)) => {
                self.cancellation.cancel();
                if self.finish_active(&subagent_id) {
                    let _ = cancel_worker(&self.backend, &subagent_id).await;
                }
                drop(permit);
                self.append_journal(&JournalEntry {
                    event: "agent_cancelled",
                    label,
                    phase,
                    subagent_id: Some(subagent_id),
                    input_hash: Some(input_hash),
                    error: Some("workflow deadline exceeded".to_string()),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
                return NativeAgentReply::fatal("workflow deadline exceeded");
            }
            result = self.backend.backend().spawn(request) => result,
        };
        self.finish_active(&subagent_id);
        drop(permit);

        let result = match backend_result {
            Ok(result) if result.success && !result.cancelled && !result.backgrounded => result,
            Ok(result) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                let error = bounded_diagnostic(result.error.unwrap_or_else(|| {
                    if result.cancelled {
                        "workflow worker was cancelled".to_string()
                    } else if result.backgrounded {
                        "workflow worker unexpectedly detached".to_string()
                    } else {
                        "workflow worker failed".to_string()
                    }
                }));
                self.append_journal(&JournalEntry {
                    event: "agent_failed",
                    label,
                    phase,
                    subagent_id: Some(subagent_id),
                    input_hash: Some(input_hash),
                    error: Some(error.clone()),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
                return NativeAgentReply::worker_failure(error);
            }
            Err(error) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                let error = bounded_diagnostic(error.to_string());
                self.append_journal(&JournalEntry {
                    event: "agent_failed",
                    label,
                    phase,
                    subagent_id: Some(subagent_id),
                    input_hash: Some(input_hash),
                    error: Some(error.clone()),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });
                return NativeAgentReply::worker_failure(error);
            }
        };

        if result.output.len() > MAX_RESULT_BYTES {
            self.failed.fetch_add(1, Ordering::Relaxed);
            let error =
                format!("worker output exceeds the {MAX_RESULT_BYTES}-byte proof-of-concept limit");
            self.append_journal(&JournalEntry {
                event: "agent_failed",
                label,
                phase,
                subagent_id: Some(subagent_id),
                input_hash: Some(input_hash),
                error: Some(error.clone()),
                timestamp: chrono::Utc::now().to_rfc3339(),
            });
            return NativeAgentReply::worker_failure(error);
        }

        let value = if let Some(schema) = options.schema.as_ref() {
            match serde_json::from_str::<Value>(&result.output) {
                Ok(value) => {
                    let validator = match jsonschema::validator_for(schema) {
                        Ok(validator) => validator,
                        Err(error) => {
                            self.failed.fetch_add(1, Ordering::Relaxed);
                            let error = format!("worker schema could not be recompiled: {error}");
                            self.append_journal(&JournalEntry {
                                event: "agent_failed",
                                label,
                                phase,
                                subagent_id: Some(subagent_id),
                                input_hash: Some(input_hash),
                                error: Some(error.clone()),
                                timestamp: chrono::Utc::now().to_rfc3339(),
                            });
                            return NativeAgentReply::worker_failure(error);
                        }
                    };
                    if let Err(validation_error) = validator.validate(&value) {
                        self.failed.fetch_add(1, Ordering::Relaxed);
                        let error = format!(
                            "worker output does not match the required schema: {validation_error}"
                        );
                        self.append_journal(&JournalEntry {
                            event: "agent_failed",
                            label,
                            phase,
                            subagent_id: Some(subagent_id),
                            input_hash: Some(input_hash),
                            error: Some(error.clone()),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                        });
                        return NativeAgentReply::worker_failure(error);
                    }
                    value
                }
                Err(error) => {
                    self.failed.fetch_add(1, Ordering::Relaxed);
                    let error = format!("worker returned invalid structured output: {error}");
                    self.append_journal(&JournalEntry {
                        event: "agent_failed",
                        label,
                        phase,
                        subagent_id: Some(subagent_id),
                        input_hash: Some(input_hash),
                        error: Some(error.clone()),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                    });
                    return NativeAgentReply::worker_failure(error);
                }
            }
        } else {
            Value::String(result.output.to_string())
        };
        let value_size = serde_json::to_vec(&value).map_or(usize::MAX, |bytes| bytes.len());
        if value_size > MAX_RESULT_BYTES {
            self.failed.fetch_add(1, Ordering::Relaxed);
            let error =
                format!("worker value exceeds the {MAX_RESULT_BYTES}-byte proof-of-concept limit");
            self.append_journal(&JournalEntry {
                event: "agent_failed",
                label,
                phase,
                subagent_id: Some(subagent_id),
                input_hash: Some(input_hash),
                error: Some(error.clone()),
                timestamp: chrono::Utc::now().to_rfc3339(),
            });
            return NativeAgentReply::worker_failure(error);
        }
        self.completed.fetch_add(1, Ordering::Relaxed);
        self.tokens_used
            .fetch_add(result.tokens_used, Ordering::Relaxed);
        if let Some(label) = label.as_deref() {
            self.persist_cached(
                label,
                &input_hash,
                &value,
                &result.subagent_id,
                result.tokens_used,
            );
        }
        self.append_journal(&JournalEntry {
            event: "agent_completed",
            label: label.clone(),
            phase: phase.clone(),
            subagent_id: Some(subagent_id.clone()),
            input_hash: Some(input_hash),
            error: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
        });
        if let Some(worktree_path) = result.worktree_path.as_deref() {
            self.append_journal(&serde_json::json!({
                "event": "worker_worktree",
                "label": label,
                "phase": phase,
                "subagent_id": subagent_id,
                "worktree_path": worktree_path,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }));
        }
        NativeAgentReply::value(value)
    }

    fn reserve_invocation_slot(&self) -> Option<usize> {
        self.invocations
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current < self.max_agents).then_some(current + 1)
            })
            .ok()
            .map(|previous| previous + 1)
    }

    fn finish_active(&self, id: &str) -> bool {
        let removed = self
            .active_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        if removed {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        removed
    }

    async fn cancel_all_workers(&self) {
        let active = {
            let mut ids = self.active_ids.lock().unwrap_or_else(|e| e.into_inner());
            ids.drain().collect::<Vec<_>>()
        };
        self.active.store(0, Ordering::SeqCst);
        let outcomes =
            futures::future::join_all(active.iter().map(|id| cancel_worker(&self.backend, id)))
                .await;
        for (id, cancelled) in active.into_iter().zip(outcomes) {
            self.append_journal(&JournalEntry {
                event: "agent_cancelled",
                label: None,
                phase: self
                    .current_phase
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                subagent_id: Some(id),
                input_hash: None,
                error: Some(
                    if cancelled {
                        "workflow stopped before worker completed"
                    } else {
                        "workflow stopped; worker cancellation could not be confirmed"
                    }
                    .to_string(),
                ),
                timestamp: chrono::Utc::now().to_rfc3339(),
            });
        }
    }

    fn set_phase(&self, phase: String) {
        let phase = phase.trim();
        if phase.is_empty() || phase.len() > 80 {
            return;
        }
        let phase = phase.to_string();
        let mut phases = self.phases.lock().unwrap_or_else(|e| e.into_inner());
        if phases.last() == Some(&phase) || phases.len() >= MAX_PHASE_EVENTS {
            return;
        }
        phases.push(phase.clone());
        drop(phases);
        *self.current_phase.lock().unwrap_or_else(|e| e.into_inner()) = Some(phase.clone());
        self.append_journal(&JournalEntry {
            event: "phase",
            label: None,
            phase: Some(phase),
            subagent_id: None,
            input_hash: None,
            error: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
        });
    }

    fn cache_path(&self, label: &str) -> PathBuf {
        self.run_dir
            .join("results")
            .join(format!("{}.json", stable_hash(label.as_bytes())))
    }

    fn read_cached(&self, label: &str, input_hash: &str) -> Option<Value> {
        let cached: PersistedAgentResult =
            read_json_bounded(&self.cache_path(label), MAX_CACHE_RECORD_BYTES).ok()?;
        if serde_json::to_vec(&cached.value).ok()?.len() > MAX_RESULT_BYTES {
            return None;
        }
        (cached.label == label && cached.input_hash == input_hash).then_some(cached.value)
    }

    fn persist_cached(
        &self,
        label: &str,
        input_hash: &str,
        value: &Value,
        subagent_id: &str,
        tokens_used: u64,
    ) {
        let persisted = PersistedAgentResult {
            label: label.to_string(),
            input_hash: input_hash.to_string(),
            value: value.clone(),
            subagent_id: subagent_id.to_string(),
            tokens_used,
        };
        let bytes = match serde_json::to_vec(&persisted) {
            Ok(bytes) if bytes.len() <= MAX_CACHE_RECORD_BYTES => bytes,
            Ok(bytes) => {
                tracing::warn!(
                    label,
                    size = bytes.len(),
                    limit = MAX_CACHE_RECORD_BYTES,
                    "workflow agent cache record exceeds its size limit"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(label, "failed to serialize workflow agent result: {error}");
                return;
            }
        };
        if let Err(error) = write_atomic(&self.cache_path(label), &bytes) {
            tracing::warn!(label, "failed to persist workflow agent result: {error}");
        }
    }

    fn append_journal(&self, entry: &impl Serialize) {
        let _guard = self.journal_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.run_dir.join("journal.jsonl");
        let serialized = match serde_json::to_string(entry) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("failed to serialize workflow journal entry: {error}");
                return;
            }
        };
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| {
                file.write_all(serialized.as_bytes())?;
                file.write_all(b"\n")?;
                file.flush()
            });
        if let Err(error) = result {
            tracing::warn!(path = %path.display(), "failed to append workflow journal: {error}");
        }
    }

    fn finish(&self, result: Value) -> Result<Value, String> {
        let result_bytes = serde_json::to_vec(&result)
            .map_err(|e| format!("workflow result is not JSON-serializable: {e}"))?;
        if result_bytes.len() > MAX_RESULT_BYTES {
            return Err(format!(
                "workflow result exceeds the {MAX_RESULT_BYTES}-byte proof-of-concept limit"
            ));
        }
        let failed = self.failed.load(Ordering::Relaxed);
        let status = if failed == 0 { "completed" } else { "partial" };
        let summary = json!({
            "status": status,
            "run_id": self.run_id,
            "result": result,
            "agents_requested": self.invocations.load(Ordering::Relaxed),
            "agents_spawned": self.spawned.load(Ordering::Relaxed),
            "agents_completed": self.completed.load(Ordering::Relaxed),
            "agents_failed": failed,
            "cache_hits": self.cache_hits.load(Ordering::Relaxed),
            "max_active_agents": self.max_active.load(Ordering::Relaxed),
            "tokens_used": self.tokens_used.load(Ordering::Relaxed),
            "max_tokens": self.max_tokens,
            "max_agents": self.max_agents,
            "max_concurrency": self.max_concurrency,
            "script_hash": self.script_hash,
            "duration_ms": self.started_at.elapsed().as_millis() as u64,
            "phases": self.phases.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            "run_dir": self.run_dir,
        });
        write_json_atomic(&self.run_dir.join("result.json"), &summary)?;
        self.append_journal(&JournalEntry {
            event: status,
            label: None,
            phase: None,
            subagent_id: None,
            input_hash: None,
            error: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
        });
        Ok(summary)
    }

    pub(crate) fn snapshot(
        &self,
        status: supervisor::WorkflowRunStatus,
        error: Option<String>,
    ) -> supervisor::WorkflowRunSnapshot {
        supervisor::WorkflowRunSnapshot {
            run_id: self.run_id.clone(),
            status,
            current_phase: self
                .current_phase
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            phases: self
                .phases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            agents_requested: self.invocations.load(Ordering::Relaxed),
            agents_spawned: self.spawned.load(Ordering::Relaxed),
            agents_completed: self.completed.load(Ordering::Relaxed),
            agents_failed: self.failed.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            active_agents: self.active.load(Ordering::Relaxed),
            max_active_agents: self.max_active.load(Ordering::Relaxed),
            max_concurrency: self.max_concurrency,
            max_agents: self.max_agents,
            tokens_used: self.tokens_used.load(Ordering::Relaxed),
            max_tokens: self.max_tokens,
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            script_hash: self.script_hash.clone(),
            created_at: self.created_at.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            run_dir: self.run_dir.clone(),
            error,
        }
    }

    fn persist_failure(&self, error: &str) {
        let summary = json!({
            "status": "failed",
            "run_id": self.run_id,
            "error": error,
            "agents_requested": self.invocations.load(Ordering::Relaxed),
            "agents_spawned": self.spawned.load(Ordering::Relaxed),
            "agents_completed": self.completed.load(Ordering::Relaxed),
            "agents_failed": self.failed.load(Ordering::Relaxed),
            "cache_hits": self.cache_hits.load(Ordering::Relaxed),
            "max_active_agents": self.max_active.load(Ordering::Relaxed),
            "tokens_used": self.tokens_used.load(Ordering::Relaxed),
            "max_tokens": self.max_tokens,
            "max_agents": self.max_agents,
            "max_concurrency": self.max_concurrency,
            "script_hash": self.script_hash,
            "duration_ms": self.started_at.elapsed().as_millis() as u64,
            "phases": self.phases.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            "run_dir": self.run_dir,
        });
        if let Err(write_error) = write_json_atomic(&self.run_dir.join("result.json"), &summary) {
            tracing::warn!("failed to persist workflow failure: {write_error}");
        }
        self.append_journal(&JournalEntry {
            event: "failed",
            label: None,
            phase: None,
            subagent_id: None,
            input_hash: None,
            error: Some(error.to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
        });
    }
}

async fn execute_javascript(
    state: Arc<WorkflowState>,
    script: String,
    args_json: String,
    timeout: Duration,
) -> Result<Value, String> {
    let runtime = AsyncRuntime::new().map_err(|e| format!("QuickJS init failed: {e}"))?;
    runtime.set_memory_limit(JS_MEMORY_LIMIT).await;
    runtime.set_max_stack_size(JS_STACK_LIMIT).await;
    let deadline = Instant::now() + timeout;
    let interrupt_cancellation = state.cancellation.clone();
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            interrupt_cancellation.is_cancelled() || Instant::now() >= deadline
        })))
        .await;
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|e| format!("QuickJS context init failed: {e}"))?;

    let wrapped = format!(
        "(async () => {{\n\
         const __workflowValue = await (async () => {{\n{script}\n}})();\n\
         return JSON.stringify(__workflowValue === undefined ? null : __workflowValue);\n\
         }})()"
    );
    let js_state = state.clone();
    let execution = {
        let execution_future = context.async_with(async move |ctx| {
            let agent_state = js_state.clone();
            ctx.globals()
                .set(
                    "__nativeAgent",
                    Func::from(Async(move |prompt: String, options: String| {
                        let state = agent_state.clone();
                        let admitted = state.admit_agent_call(prompt, options);
                        async move {
                            match admitted {
                                Ok((prompt, options_json, options, invocation_index)) => {
                                    let settlement_state = state.clone();
                                    let reply = state
                                        .execute_agent(
                                            prompt,
                                            options_json,
                                            options,
                                            invocation_index,
                                        )
                                        .await;
                                    settlement_state
                                        .settled_invocations
                                        .fetch_add(1, Ordering::SeqCst);
                                    reply
                                }
                                Err(error) => NativeAgentReply::fatal(error),
                            }
                        }
                    })),
                )
                .catch(&ctx)
                .map_err(|e| format!("failed to install agent binding: {e}"))?;

            let phase_state = js_state.clone();
            ctx.globals()
                .set(
                    "__nativePhase",
                    Func::from(move |phase: String| phase_state.set_phase(phase)),
                )
                .catch(&ctx)
                .map_err(|e| format!("failed to install phase binding: {e}"))?;
            ctx.globals()
                .set("__workflowArgsJson", args_json)
                .catch(&ctx)
                .map_err(|e| format!("failed to install workflow args: {e}"))?;

            ctx.eval::<(), _>(WORKFLOW_BOOTSTRAP)
                .catch(&ctx)
                .map_err(|e| format!("workflow bootstrap failed: {e}"))?;
            let promise: Promise = ctx
                .eval(wrapped)
                .catch(&ctx)
                .map_err(|e| format!("workflow script did not compile: {e}"))?;
            promise
                .into_future::<String>()
                .await
                .catch(&ctx)
                .map_err(|e| format!("workflow script failed: {e}"))
        });
        tokio::pin!(execution_future);
        tokio::select! {
            biased;
            result = &mut execution_future => result,
            _ = state.cancellation.cancelled() => {
                Err("workflow was cancelled".to_string())
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                state.cancellation.cancel();
                Err("workflow deadline exceeded".to_string())
            }
        }
    };

    match execution {
        Ok(result_json) => {
            let invoked = state.invocations.load(Ordering::SeqCst);
            let settled = state.settled_invocations.load(Ordering::SeqCst);
            let active = state
                .active_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len();
            if settled != invoked || active != 0 {
                state.cancellation.cancel();
                state.cancel_all_workers().await;
                return Err(format!(
                    "workflow returned with {} unfinished agent call(s); await every agent promise",
                    invoked.saturating_sub(settled).max(active)
                ));
            }
            runtime.idle().await;
            serde_json::from_str(&result_json)
                .map_err(|e| format!("workflow returned invalid JSON: {e}"))
        }
        Err(error) => {
            let error = if Instant::now() >= deadline {
                "workflow deadline exceeded".to_string()
            } else if state.cancellation.is_cancelled() {
                "workflow was cancelled".to_string()
            } else {
                error
            };
            state.cancellation.cancel();
            state.cancel_all_workers().await;
            Err(error)
        }
    }
}

const WORKFLOW_BOOTSTRAP: &str = r#"
(() => {
  const nativeAgent = globalThis.__nativeAgent;
  const nativePhase = globalThis.__nativePhase;
  const deepFreeze = (value, seen = new Set()) => {
    if (value === null || typeof value !== "object" || seen.has(value)) return value;
    seen.add(value);
    for (const key of Object.keys(value)) deepFreeze(value[key], seen);
    return Object.freeze(value);
  };

  const parsedArgs = JSON.parse(globalThis.__workflowArgsJson);
  Object.defineProperty(globalThis, "args", {
    value: deepFreeze(parsedArgs), writable: false, configurable: false
  });

  Object.defineProperty(globalThis, "agent", {
    value: async (prompt, options = {}) => {
      if (typeof prompt !== "string") throw new TypeError("agent prompt must be a string");
      const reply = JSON.parse(await nativeAgent(prompt, JSON.stringify(options)));
      if (reply.fatal) throw new Error(reply.error || "workflow agent failed");
      return reply.value;
    },
    writable: false, configurable: false
  });

  Object.defineProperty(globalThis, "parallel", {
    value: async (branches) => {
      if (!Array.isArray(branches)) throw new TypeError("parallel expects an array");
      return Promise.all(branches.map((branch) =>
        typeof branch === "function" ? branch() : branch
      ));
    },
    writable: false, configurable: false
  });

  Object.defineProperty(globalThis, "pipeline", {
    value: async (items, ...stages) => {
      if (!Array.isArray(items)) throw new TypeError("pipeline items must be an array");
      if (stages.length === 0 || stages.some((stage) => typeof stage !== "function")) {
        throw new TypeError("pipeline requires one or more stage functions");
      }
      return Promise.all(items.map(async (original, index) => {
        let current = original;
        for (const stage of stages) {
          if (current === null || current === undefined) return null;
          current = await stage(current, original, index);
        }
        return current;
      }));
    },
    writable: false, configurable: false
  });

  Object.defineProperty(globalThis, "phase", {
    value: (name) => {
      if (typeof name !== "string") throw new TypeError("phase name must be a string");
      nativePhase(name);
    },
    writable: false, configurable: false
  });

  Object.defineProperty(Math, "random", {
    value: () => { throw new Error("Math.random is disabled in replayable workflows"); },
    writable: false, configurable: false
  });
  for (const name of [
    "Date", "eval", "Function", "WebAssembly", "performance",
    "WeakRef", "FinalizationRegistry"
  ]) {
    try {
      Object.defineProperty(globalThis, name, {
        value: undefined, writable: false, configurable: false
      });
    } catch (_) {}
  }
  delete globalThis.__nativeAgent;
  delete globalThis.__nativePhase;
  delete globalThis.__workflowArgsJson;
})();
"#;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WorkflowMetadata {
    version: u32,
    run_id: String,
    session_id: String,
    script_hash: String,
    args_hash: String,
    max_concurrency: usize,
    max_agents: usize,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    retention_days: Option<u64>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedAgentResult {
    label: String,
    input_hash: String,
    value: Value,
    subagent_id: String,
    tokens_used: u64,
}

#[derive(Serialize)]
struct JournalEntry<'a> {
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subagent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    timestamp: String,
}

fn stable_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub(crate) fn resolve_workflow_source(
    input: &mut WorkflowInput,
    cwd: &Path,
) -> Result<Option<PathBuf>, String> {
    let has_inline = !input.script.trim().is_empty();
    let saved_name = input
        .saved_workflow
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if has_inline && saved_name.is_some() {
        return Err("provide either script or saved_workflow, not both".to_string());
    }
    let Some(name) = saved_name else {
        if has_inline {
            return Ok(None);
        }
        return Err("workflow requires script or saved_workflow".to_string());
    };
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(
            "saved_workflow may contain only ASCII letters, digits, `_`, and `-`".to_string(),
        );
    }
    let filename = format!("{name}.js");
    let project_path = cwd.join(".atlas").join("workflows").join(&filename);
    let user_path =
        dirs::home_dir().map(|home| home.join(".atlas").join("workflows").join(&filename));
    let path = if project_path.is_file() {
        project_path
    } else if user_path.as_ref().is_some_and(|path| path.is_file()) {
        user_path.expect("checked above")
    } else {
        return Err(format!(
            "saved workflow '{name}' was not found in project or personal workflow directories"
        ));
    };
    let mut file = File::open(&path)
        .map_err(|error| format!("failed to open saved workflow {}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| {
        format!(
            "failed to inspect saved workflow {}: {error}",
            path.display()
        )
    })?;
    if metadata.len() > MAX_SCRIPT_BYTES as u64 {
        return Err(format!(
            "saved workflow {} exceeds the {MAX_SCRIPT_BYTES}-byte limit",
            path.display()
        ));
    }
    let mut script = String::new();
    file.read_to_string(&mut script)
        .map_err(|error| format!("failed to read saved workflow {}: {error}", path.display()))?;
    input.script = script;
    Ok(Some(path))
}

pub(crate) fn workflow_approval_hash(input: &WorkflowInput) -> Result<String, String> {
    let args = serde_json::to_vec(&input.args)
        .map_err(|error| format!("workflow args are not valid JSON: {error}"))?;
    let approval = json!({
        "version": 1,
        "script_hash": stable_hash(input.script.as_bytes()),
        "args_hash": stable_hash(&args),
        "max_concurrency": input.max_concurrency.unwrap_or(DEFAULT_CONCURRENCY),
        "max_agents": input.max_agents.unwrap_or(MAX_AGENTS),
        "max_tokens": input.max_tokens,
        "timeout_seconds": input.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
        "retention_days": input.retention_days.unwrap_or(DEFAULT_RETENTION_DAYS),
        "run_in_background": input.run_in_background,
    });
    serde_json::to_vec(&approval)
        .map(|bytes| stable_hash(&bytes))
        .map_err(|error| format!("failed to encode workflow approval: {error}"))
}

/// Keep workflow-authored schemas self-contained.
///
/// `jsonschema` enables file and HTTP retrieval by default. The child-session
/// structured-output path compiles this value later, so reject every base URI
/// and every non-fragment reference before the request can leave the isolated
/// workflow runtime.
fn validate_workflow_schema(schema: &Value) -> Result<(), String> {
    if !schema.is_object() {
        return Err("agent schema must be a JSON object".to_string());
    }

    fn walk(value: &Value, path: &str) -> Result<(), String> {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    let child_path = format!("{path}/{key}");
                    match key.as_str() {
                        "$id" | "$schema" => {
                            return Err(format!(
                                "workflow agent schemas cannot contain `{key}` ({child_path})"
                            ));
                        }
                        "$ref" | "$dynamicRef" | "$recursiveRef" => {
                            let reference = child.as_str().ok_or_else(|| {
                                format!("`{key}` must be a string ({child_path})")
                            })?;
                            if !reference.starts_with('#') {
                                return Err(format!(
                                    "workflow agent schemas only allow local fragment `{key}` values ({child_path})"
                                ));
                            }
                        }
                        _ => {}
                    }
                    walk(child, &child_path)?;
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    walk(child, &format!("{path}/{index}"))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    walk(schema, "$")
}

/// Project a valid workflow schema into the native provider's narrower
/// structured-output dialect.
///
/// `uniqueItems` is standard JSON Schema and remains enforced by the local
/// validator before a worker result is completed or cached. OpenAI-compatible
/// native response formats reject the keyword at request admission, however,
/// so it must not cross that boundary.
fn provider_compatible_schema(schema: &Value) -> Value {
    fn project_schema_map(value: &Value) -> Value {
        let Some(entries) = value.as_object() else {
            return value.clone();
        };
        Value::Object(
            entries
                .iter()
                .map(|(name, child_schema)| {
                    (name.clone(), provider_compatible_schema(child_schema))
                })
                .collect(),
        )
    }

    let Value::Object(object) = schema else {
        return schema.clone();
    };
    Value::Object(
        object
            .iter()
            .filter_map(|(key, value)| {
                if key == "uniqueItems" {
                    return None;
                }
                let projected = match key.as_str() {
                    // These values are JSON instances rather than schemas.
                    // Preserve an object member literally named `uniqueItems`.
                    "const" | "default" | "enum" | "examples" => value.clone(),
                    // Keys in schema maps are user-defined property/definition
                    // names. Only the values are schemas.
                    "properties" | "patternProperties" | "$defs" | "definitions"
                    | "dependentSchemas" => project_schema_map(value),
                    _ => match value {
                        Value::Object(_) => provider_compatible_schema(value),
                        Value::Array(items) => Value::Array(
                            items
                                .iter()
                                .map(|item| match item {
                                    Value::Object(_) => provider_compatible_schema(item),
                                    _ => item.clone(),
                                })
                                .collect(),
                        ),
                        _ => value.clone(),
                    },
                };
                Some((key.clone(), projected))
            })
            .collect(),
    )
}

/// Accept the intentionally narrow POC source form and replace its metadata
/// initializer with an inert object.
///
/// Metadata is declarative, not part of the execution graph. Removing the
/// initializer before evaluation means even a malformed "literal" containing
/// getters, computed keys, or `await agent(...)` cannot run hidden work.
fn transform_workflow_script(script: &str) -> Result<String, String> {
    const EXPORT: &str = "export const meta";
    let leading = script.len() - script.trim_start().len();
    let trimmed = &script[leading..];
    let Some(after_export) = trimmed.strip_prefix(EXPORT) else {
        return Err(
            "workflow script must begin with literal metadata: `export const meta = { ... }`"
                .to_string(),
        );
    };
    let after_export_trimmed = after_export.trim_start();
    let Some(after_equals) = after_export_trimmed.strip_prefix('=') else {
        return Err(
            "workflow metadata declaration must be exactly `export const meta = ...`".to_string(),
        );
    };
    let after_equals_trimmed = after_equals.trim_start();
    if !after_equals_trimmed.starts_with('{') {
        return Err("workflow metadata declaration must assign an object literal".to_string());
    }
    let object_start = script.len() - after_equals_trimmed.len();
    let object_end = object_start + metadata_object_end(after_equals_trimmed)?;
    let after_object = &script[object_end..];
    if !after_object.trim_start().starts_with(';') {
        return Err(
            "workflow metadata object must end with `;` before executable workflow code"
                .to_string(),
        );
    }

    let mut transformed = String::with_capacity(script.len());
    transformed.push_str(&script[..leading]);
    transformed.push_str("const meta = {}");
    transformed.push_str(after_object);
    Ok(transformed)
}

/// Return the byte length of the first balanced object literal.
///
/// Single- and double-quoted strings are supported. Template strings and
/// comments are intentionally rejected in metadata to keep this scanner small
/// and deterministic.
fn metadata_object_end(source: &str) -> Result<usize, String> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, byte) in bytes.iter().copied().enumerate() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active_quote {
                quote = None;
            }
            continue;
        }

        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'`' => {
                return Err(
                    "workflow metadata does not allow template strings; use quoted literals"
                        .to_string(),
                );
            }
            b'/' => {
                return Err(
                    "workflow metadata does not allow comments or expressions containing `/`"
                        .to_string(),
                );
            }
            b'{' => depth += 1,
            b'}' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| "workflow metadata has an unmatched `}`".to_string())?;
                if depth == 0 {
                    return Ok(index + 1);
                }
            }
            _ => {}
        }
    }

    Err("workflow metadata object is not closed".to_string())
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid workflow path: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7()));
    fs::write(&temporary, bytes)
        .map_err(|e| format!("failed to write {}: {e}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|e| {
        let _ = fs::remove_file(&temporary);
        format!("failed to replace {}: {e}", path.display())
    })
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    write_atomic(path, &bytes)
}

#[cfg(test)]
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

fn read_json_bounded<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max_bytes: usize,
) -> Result<T, String> {
    let file = File::open(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "{} exceeds the {max_bytes}-byte workflow cache limit",
            path.display()
        ));
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

fn bounded_diagnostic(mut value: String) -> String {
    if value.len() <= MAX_DIAGNOSTIC_BYTES {
        return value;
    }
    const SUFFIX: &str = "… [truncated]";
    let mut boundary = MAX_DIAGNOSTIC_BYTES - SUFFIX.len();
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str(SUFFIX);
    value
}

/// Best-effort cancellation helper shared by deadline and caller cancellation.
///
/// The channel backend may receive Cancel just after it dequeues Spawn but
/// before the spawned handler registers the child. Brief retries bridge that
/// scheduling window; the handler also refuses abandoned quiet requests.
async fn cancel_worker(backend: &SubagentBackendResource, id: &str) -> bool {
    const ATTEMPTS: usize = 20;
    for attempt in 0..ATTEMPTS {
        match tokio::time::timeout(Duration::from_millis(50), backend.backend().cancel(id)).await {
            Ok(
                SubagentCancelOutcome::Cancelled | SubagentCancelOutcome::AlreadyFinished { .. },
            ) => {
                return true;
            }
            Ok(SubagentCancelOutcome::NotFound) | Err(_) if attempt + 1 < ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok(SubagentCancelOutcome::NotFound) | Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests;
