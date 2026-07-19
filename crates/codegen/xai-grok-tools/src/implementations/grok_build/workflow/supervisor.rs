use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::notification::types::{ToolNotificationHandle, WorkflowProgress};

use super::{
    MAX_RESULT_BYTES, PreparedRun, WorkflowMetadata, WorkflowState, bounded_diagnostic,
    read_json_bounded, write_json_atomic,
};

const STATUS_FILE: &str = "status.json";
const RESULT_FILE: &str = "result.json";
const ACTOR_TICK: Duration = Duration::from_secs(1);
const DEFAULT_RETENTION_DAYS: u64 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Queued,
    Running,
    Paused,
    Completed,
    Partial,
    Failed,
    Cancelled,
    Interrupted,
}

impl WorkflowRunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Paused
                | Self::Completed
                | Self::Partial
                | Self::Failed
                | Self::Cancelled
                | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunSnapshot {
    pub run_id: String,
    pub status: WorkflowRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_phase: Option<String>,
    #[serde(default)]
    pub phases: Vec<String>,
    pub agents_requested: usize,
    pub agents_spawned: usize,
    pub agents_completed: usize,
    pub agents_failed: usize,
    pub cache_hits: usize,
    pub active_agents: usize,
    pub max_active_agents: usize,
    pub max_concurrency: usize,
    pub max_agents: usize,
    pub tokens_used: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    pub duration_ms: u64,
    pub script_hash: String,
    pub created_at: String,
    pub updated_at: String,
    pub run_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WorkflowRunSnapshot {
    fn recovered(
        run_dir: PathBuf,
        metadata: WorkflowMetadata,
        status: WorkflowRunStatus,
        result: Option<&Value>,
    ) -> Self {
        let number = |key: &str| {
            result
                .and_then(|value| value.get(key))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        let phases: Vec<String> = result
            .and_then(|value| value.get("phases"))
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let error = result
            .and_then(|value| value.get("error"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        Self {
            run_id: metadata.run_id,
            status,
            current_phase: phases.last().cloned(),
            phases,
            agents_requested: number("agents_requested") as usize,
            agents_spawned: number("agents_spawned") as usize,
            agents_completed: number("agents_completed") as usize,
            agents_failed: number("agents_failed") as usize,
            cache_hits: number("cache_hits") as usize,
            active_agents: 0,
            max_active_agents: number("max_active_agents") as usize,
            max_concurrency: metadata.max_concurrency,
            max_agents: metadata.max_agents,
            tokens_used: number("tokens_used"),
            max_tokens: metadata.max_tokens,
            duration_ms: number("duration_ms"),
            script_hash: metadata.script_hash,
            created_at: metadata.created_at,
            updated_at: chrono::Utc::now().to_rfc3339(),
            run_dir,
            error,
        }
    }
}

#[derive(Clone)]
pub struct WorkflowSupervisorHandle(pub(crate) mpsc::UnboundedSender<WorkflowCommand>);

pub(crate) struct WorkflowStart {
    pub snapshot: WorkflowRunSnapshot,
    pub updates: watch::Receiver<WorkflowRunSnapshot>,
}

pub(crate) enum WorkflowCommand {
    Start {
        prepared: PreparedRun,
        reply: oneshot::Sender<Result<WorkflowStart, String>>,
    },
    List {
        reply: oneshot::Sender<Vec<WorkflowRunSnapshot>>,
    },
    Get {
        run_id: String,
        reply: oneshot::Sender<Option<WorkflowRunSnapshot>>,
    },
    IsActive {
        run_id: String,
        reply: oneshot::Sender<bool>,
    },
    Stop {
        run_id: String,
        paused: bool,
        reply: oneshot::Sender<Result<WorkflowRunSnapshot, String>>,
    },
    Completed {
        run_id: String,
        result: Result<Value, String>,
    },
}

struct ManagedRun {
    state: Arc<WorkflowState>,
    snapshot: WorkflowRunSnapshot,
    updates: watch::Sender<WorkflowRunSnapshot>,
    requested_terminal: Option<WorkflowRunStatus>,
}

pub(crate) struct WorkflowSupervisor {
    workflows_dir: PathBuf,
    notification_handle: ToolNotificationHandle,
    cmd_tx: mpsc::UnboundedSender<WorkflowCommand>,
    cmd_rx: mpsc::UnboundedReceiver<WorkflowCommand>,
    cancel_token: CancellationToken,
    runs: HashMap<String, ManagedRun>,
    recovered: HashMap<String, WorkflowRunSnapshot>,
}

impl WorkflowSupervisor {
    pub(crate) fn start(
        workflows_dir: PathBuf,
        notification_handle: ToolNotificationHandle,
        cancel_token: CancellationToken,
    ) -> WorkflowSupervisorHandle {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let handle = WorkflowSupervisorHandle(cmd_tx.clone());
        let actor = Self {
            workflows_dir,
            notification_handle,
            cmd_tx,
            cmd_rx,
            cancel_token,
            runs: HashMap::new(),
            recovered: HashMap::new(),
        };
        tokio::spawn(actor.run());
        handle
    }

    async fn run(mut self) {
        self.recover_and_prune();
        let mut tick = tokio::time::interval(ACTOR_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => {
                    let mut interrupted = Vec::new();
                    for run in self.runs.values_mut() {
                        run.state.cancellation.cancel();
                        run.snapshot = run.state.snapshot(
                            WorkflowRunStatus::Interrupted,
                            Some("owning Atlas session stopped".to_string()),
                        );
                        let snapshot = run.snapshot.clone();
                        let _ = run.updates.send(snapshot.clone());
                        interrupted.push(snapshot);
                    }
                    for snapshot in interrupted {
                        self.persist_and_notify(&snapshot);
                    }
                    break;
                }
                Some(command) = self.cmd_rx.recv() => self.handle(command).await,
                _ = tick.tick() => self.refresh_running(),
            }
        }
    }

    async fn handle(&mut self, command: WorkflowCommand) {
        match command {
            WorkflowCommand::Start { prepared, reply } => {
                let run_id = prepared.state.run_id.clone();
                if self.runs.contains_key(&run_id) {
                    let _ = reply.send(Err(format!("workflow run '{run_id}' is already active")));
                    return;
                }
                let snapshot = prepared.state.snapshot(WorkflowRunStatus::Running, None);
                tracing::info!(
                    run_id,
                    max_agents = snapshot.max_agents,
                    max_concurrency = snapshot.max_concurrency,
                    "workflow run started"
                );
                let (updates, receiver) = watch::channel(snapshot.clone());
                let state = prepared.state.clone();
                self.persist_and_notify(&snapshot);
                self.runs.insert(
                    run_id.clone(),
                    ManagedRun {
                        state,
                        snapshot: snapshot.clone(),
                        updates,
                        requested_terminal: None,
                    },
                );
                self.recovered.remove(&run_id);
                let completion_tx = self.cmd_tx.clone();
                let launch_id = run_id.clone();
                let launch_result = launch(prepared, move |result| {
                    let _ = completion_tx.send(WorkflowCommand::Completed {
                        run_id: launch_id,
                        result,
                    });
                });
                if let Err(error) = launch_result {
                    let _ = self.cmd_tx.send(WorkflowCommand::Completed {
                        run_id: run_id.clone(),
                        result: Err(error),
                    });
                }
                let _ = reply.send(Ok(WorkflowStart {
                    snapshot,
                    updates: receiver,
                }));
            }
            WorkflowCommand::List { reply } => {
                let mut snapshots = self
                    .recovered
                    .values()
                    .cloned()
                    .chain(self.runs.values().map(|run| run.snapshot.clone()))
                    .collect::<Vec<_>>();
                snapshots.sort_by(|left, right| right.created_at.cmp(&left.created_at));
                let _ = reply.send(snapshots);
            }
            WorkflowCommand::Get { run_id, reply } => {
                let snapshot = self
                    .runs
                    .get(&run_id)
                    .map(|run| run.snapshot.clone())
                    .or_else(|| self.recovered.get(&run_id).cloned());
                let _ = reply.send(snapshot);
            }
            WorkflowCommand::IsActive { run_id, reply } => {
                let _ = reply.send(self.runs.contains_key(&run_id));
            }
            WorkflowCommand::Stop {
                run_id,
                paused,
                reply,
            } => {
                let Some(run) = self.runs.get_mut(&run_id) else {
                    let _ = reply.send(Err(format!("workflow run '{run_id}' is not active")));
                    return;
                };
                let status = if paused {
                    WorkflowRunStatus::Paused
                } else {
                    WorkflowRunStatus::Cancelled
                };
                run.requested_terminal = Some(status);
                run.state.cancellation.cancel();
                run.snapshot = run.state.snapshot(status, None);
                let snapshot = run.snapshot.clone();
                let _ = run.updates.send(snapshot.clone());
                self.persist_and_notify(&snapshot);
                let _ = reply.send(Ok(snapshot));
            }
            WorkflowCommand::Completed { run_id, result } => {
                let Some(mut run) = self.runs.remove(&run_id) else {
                    return;
                };
                let (status, error) = match run.requested_terminal {
                    Some(status) => (status, None),
                    None => match &result {
                        Ok(value) => {
                            let status = match value.get("status").and_then(Value::as_str) {
                                Some("partial") => WorkflowRunStatus::Partial,
                                _ => WorkflowRunStatus::Completed,
                            };
                            (status, None)
                        }
                        Err(error) => (WorkflowRunStatus::Failed, Some(error.clone())),
                    },
                };
                run.snapshot = run.state.snapshot(status, error);
                let snapshot = run.snapshot.clone();
                tracing::info!(
                    run_id,
                    status = ?snapshot.status,
                    agents_spawned = snapshot.agents_spawned,
                    agents_failed = snapshot.agents_failed,
                    tokens_used = snapshot.tokens_used,
                    duration_ms = snapshot.duration_ms,
                    "workflow run finished"
                );
                let _ = run.updates.send(snapshot.clone());
                self.persist_and_notify(&snapshot);
                self.recovered.insert(run_id, snapshot);
            }
        }
    }

    fn refresh_running(&mut self) {
        let mut changed = Vec::new();
        for run in self.runs.values_mut() {
            if run.requested_terminal.is_some() {
                continue;
            }
            run.snapshot = run.state.snapshot(WorkflowRunStatus::Running, None);
            let snapshot = run.snapshot.clone();
            let _ = run.updates.send(snapshot.clone());
            changed.push(snapshot);
        }
        for snapshot in changed {
            self.persist_and_notify(&snapshot);
        }
    }

    fn persist_and_notify(&self, snapshot: &WorkflowRunSnapshot) {
        if let Err(error) = write_json_atomic(&snapshot.run_dir.join(STATUS_FILE), snapshot) {
            tracing::warn!(
                run_id = %snapshot.run_id,
                "failed to persist workflow status: {error}"
            );
        }
        self.notification_handle
            .send_workflow_progress(WorkflowProgress {
                snapshot: snapshot.clone(),
            });
    }

    fn recover_and_prune(&mut self) {
        let Ok(entries) = fs::read_dir(&self.workflows_dir) else {
            return;
        };
        let now = chrono::Utc::now();
        for entry in entries.flatten() {
            let run_dir = entry.path();
            if !run_dir.is_dir() {
                continue;
            }
            let Ok(metadata) =
                read_json_bounded::<WorkflowMetadata>(&run_dir.join("metadata.json"), 64 * 1024)
            else {
                continue;
            };
            let created = chrono::DateTime::parse_from_rfc3339(&metadata.created_at)
                .ok()
                .map(|value| value.with_timezone(&chrono::Utc));
            let retention_days = metadata.retention_days.unwrap_or(DEFAULT_RETENTION_DAYS);
            if created.is_some_and(|created| {
                now.signed_duration_since(created).num_days() >= retention_days as i64
            }) {
                if let Err(error) = fs::remove_dir_all(&run_dir) {
                    tracing::warn!(path = %run_dir.display(), "failed to prune workflow run: {error}");
                }
                continue;
            }
            let result = read_json_bounded::<Value>(
                &run_dir.join(RESULT_FILE),
                MAX_RESULT_BYTES + 64 * 1024,
            )
            .ok();
            let persisted_status =
                read_json_bounded::<WorkflowRunSnapshot>(&run_dir.join(STATUS_FILE), 128 * 1024)
                    .ok()
                    .map(|snapshot| snapshot.status);
            let result_status = result
                .as_ref()
                .and_then(|value| value.get("status"))
                .and_then(Value::as_str)
                .map(|status| match status {
                    "completed" => WorkflowRunStatus::Completed,
                    "partial" => WorkflowRunStatus::Partial,
                    "cancelled" => WorkflowRunStatus::Cancelled,
                    "paused" => WorkflowRunStatus::Paused,
                    _ => WorkflowRunStatus::Failed,
                });
            let status = match persisted_status {
                Some(status) if status.is_terminal() => status,
                Some(WorkflowRunStatus::Queued | WorkflowRunStatus::Running) | None => {
                    result_status.unwrap_or(WorkflowRunStatus::Interrupted)
                }
                Some(status) => status,
            };
            let snapshot =
                WorkflowRunSnapshot::recovered(run_dir, metadata, status, result.as_ref());
            tracing::info!(
                run_id = %snapshot.run_id,
                status = ?snapshot.status,
                "recovered persisted workflow run"
            );
            self.persist_and_notify(&snapshot);
            self.recovered.insert(snapshot.run_id.clone(), snapshot);
        }
    }
}

fn launch(
    prepared: PreparedRun,
    on_complete: impl FnOnce(Result<Value, String>) + Send + 'static,
) -> Result<(), String> {
    let state = prepared.state.clone();
    let script = prepared.script;
    let args_json = prepared.args_json;
    let timeout = prepared.timeout;
    let name = format!("workflow-{}", super::short_id(&state.run_id));
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let execution = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .map_err(|error| format!("failed to create workflow runtime: {error}"))
                .and_then(|runtime| {
                    let local = tokio::task::LocalSet::new();
                    local.block_on(
                        &runtime,
                        super::execute_javascript(state.clone(), script, args_json, timeout),
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
            on_complete(result);
        })
        .map(|_| ())
        .map_err(|error| format!("failed to start workflow runtime: {error}"))
}

pub(crate) async fn start(
    handle: &WorkflowSupervisorHandle,
    prepared: PreparedRun,
) -> Result<WorkflowStart, String> {
    let (reply, receive) = oneshot::channel();
    handle
        .0
        .send(WorkflowCommand::Start { prepared, reply })
        .map_err(|_| "workflow supervisor is unavailable".to_string())?;
    receive
        .await
        .map_err(|_| "workflow supervisor stopped before starting the run".to_string())?
}

pub async fn list(handle: &WorkflowSupervisorHandle) -> Result<Vec<WorkflowRunSnapshot>, String> {
    let (reply, receive) = oneshot::channel();
    handle
        .0
        .send(WorkflowCommand::List { reply })
        .map_err(|_| "workflow supervisor is unavailable".to_string())?;
    receive
        .await
        .map_err(|_| "workflow supervisor stopped while listing runs".to_string())
}

pub async fn get(
    handle: &WorkflowSupervisorHandle,
    run_id: String,
) -> Result<Option<WorkflowRunSnapshot>, String> {
    let (reply, receive) = oneshot::channel();
    handle
        .0
        .send(WorkflowCommand::Get { run_id, reply })
        .map_err(|_| "workflow supervisor is unavailable".to_string())?;
    receive
        .await
        .map_err(|_| "workflow supervisor stopped while inspecting a run".to_string())
}

pub async fn stop(
    handle: &WorkflowSupervisorHandle,
    run_id: String,
    paused: bool,
) -> Result<WorkflowRunSnapshot, String> {
    let (reply, receive) = oneshot::channel();
    handle
        .0
        .send(WorkflowCommand::Stop {
            run_id,
            paused,
            reply,
        })
        .map_err(|_| "workflow supervisor is unavailable".to_string())?;
    receive
        .await
        .map_err(|_| "workflow supervisor stopped while controlling a run".to_string())?
}

pub async fn wait_until_inactive(
    handle: &WorkflowSupervisorHandle,
    run_id: &str,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (reply, receive) = oneshot::channel();
        handle
            .0
            .send(WorkflowCommand::IsActive {
                run_id: run_id.to_string(),
                reply,
            })
            .map_err(|_| "workflow supervisor is unavailable".to_string())?;
        if !receive
            .await
            .map_err(|_| "workflow supervisor stopped while waiting for workers".to_string())?
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "workflow run '{run_id}' did not stop within 30 seconds"
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(crate) async fn wait_for_terminal(
    mut updates: watch::Receiver<WorkflowRunSnapshot>,
) -> Result<WorkflowRunSnapshot, String> {
    loop {
        let snapshot = updates.borrow().clone();
        if snapshot.status.is_terminal() {
            return Ok(snapshot);
        }
        updates
            .changed()
            .await
            .map_err(|_| "workflow supervisor stopped before the run completed".to_string())?;
    }
}

pub fn validate_run_id(raw: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(raw.trim())
        .map(|id| id.to_string())
        .map_err(|_| "run_id must be a workflow UUID".to_string())
}

pub fn run_dir(session_folder: &Path, run_id: &str) -> PathBuf {
    session_folder.join("workflows").join(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_marks_orphaned_run_interrupted() {
        let temp = tempfile::tempdir().unwrap();
        let workflows = temp.path().join("workflows");
        let run_id = uuid::Uuid::now_v7().to_string();
        let run_dir = workflows.join(&run_id);
        fs::create_dir_all(&run_dir).unwrap();
        let metadata = WorkflowMetadata {
            version: 1,
            run_id: run_id.clone(),
            session_id: "session".to_string(),
            script_hash: "script".to_string(),
            args_hash: "args".to_string(),
            max_concurrency: 2,
            max_agents: 8,
            max_tokens: Some(10_000),
            retention_days: Some(7),
            timeout_seconds: Some(60),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        write_json_atomic(&run_dir.join("metadata.json"), &metadata).unwrap();

        let cancellation = CancellationToken::new();
        let handle = WorkflowSupervisor::start(
            workflows,
            ToolNotificationHandle::noop(),
            cancellation.clone(),
        );
        tokio::task::yield_now().await;
        let runs = list(&handle).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, run_id);
        assert_eq!(runs[0].status, WorkflowRunStatus::Interrupted);
        assert!(run_dir.join(STATUS_FILE).is_file());
        cancellation.cancel();
    }
}
