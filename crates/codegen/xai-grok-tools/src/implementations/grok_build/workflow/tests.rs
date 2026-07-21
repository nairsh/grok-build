use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;

use super::*;
use crate::implementations::grok_build::task::backend::SubagentBackend;
use crate::implementations::grok_build::task::types::{
    SubagentCancelOutcome, SubagentDescribeOutcome, SubagentResult, SubagentSnapshot,
    SubagentTypeSummary, SubagentValidateTypeOutcome,
};
use crate::types::resources::Resources;
use crate::types::tool_metadata::test_ctx;

struct FakeBackend {
    requests: Mutex<Vec<RequestSnapshot>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
    spawn_count: AtomicUsize,
    cancel_not_found_remaining: AtomicUsize,
    block_spawns: AtomicBool,
    spawn_started: tokio::sync::Semaphore,
    cancellation_observed: tokio::sync::Semaphore,
    cancelled_ids: Mutex<Vec<String>>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            spawn_count: AtomicUsize::new(0),
            cancel_not_found_remaining: AtomicUsize::new(0),
            block_spawns: AtomicBool::new(false),
            spawn_started: tokio::sync::Semaphore::new(0),
            cancellation_observed: tokio::sync::Semaphore::new(0),
            cancelled_ids: Mutex::new(Vec::new()),
        }
    }
}

#[derive(Debug)]
struct RequestSnapshot {
    prompt: String,
    json_schema: Option<Value>,
    run_in_background: bool,
    surface_completion: bool,
    fork_context: bool,
    strict_read_only: bool,
    strict_workflow_write: bool,
    capability_mode: Option<SubagentCapabilityMode>,
    isolation: Option<xai_tool_types::SubagentIsolationMode>,
}

#[async_trait]
impl SubagentBackend for FakeBackend {
    async fn spawn(
        &self,
        request: SubagentRequest,
    ) -> Result<SubagentResult, xai_tool_runtime::ToolError> {
        self.spawn_count.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(RequestSnapshot {
                prompt: request.prompt.clone(),
                json_schema: request.json_schema.clone(),
                run_in_background: request.run_in_background,
                surface_completion: request.surface_completion,
                fork_context: request.fork_context,
                strict_read_only: request.strict_read_only,
                strict_workflow_write: request.strict_workflow_write,
                capability_mode: request.runtime_overrides.capability_mode,
                isolation: request.runtime_overrides.isolation,
            });
        let should_block = self.block_spawns.load(Ordering::SeqCst);
        if request.prompt == "arm-next" {
            self.block_spawns.store(true, Ordering::SeqCst);
        }
        if should_block {
            self.spawn_started.add_permits(1);
            std::future::pending::<()>().await;
        }
        let delay = if request.prompt == "stage1:slow" {
            Duration::from_millis(150)
        } else {
            Duration::from_millis(15)
        };
        tokio::time::sleep(delay).await;
        self.active.fetch_sub(1, Ordering::SeqCst);

        if request.prompt == "oversized-error" {
            return Ok(SubagentResult {
                success: false,
                error: Some("e".repeat(MAX_DIAGNOSTIC_BYTES * 2)),
                subagent_id: request.id.clone(),
                child_session_id: request.id,
                ..Default::default()
            });
        }
        if request.prompt.contains("intentional-failure") {
            return Ok(SubagentResult {
                success: false,
                error: Some("intentional worker failure".to_string()),
                subagent_id: request.id.clone(),
                child_session_id: request.id,
                ..Default::default()
            });
        }
        let output = if request.prompt == "oversized-output" {
            "x".repeat(MAX_RESULT_BYTES + 1)
        } else if request.prompt == "boundary-output" {
            "x".repeat(MAX_RESULT_BYTES - 2)
        } else if request.json_schema.is_some() && request.prompt.contains("wrong-shape") {
            json!({ "unexpected": true }).to_string()
        } else if request.json_schema.is_some() {
            let item = request
                .prompt
                .strip_prefix("inspect:")
                .unwrap_or(&request.prompt);
            json!({ "item": item, "ok": true }).to_string()
        } else {
            format!("result:{}", request.prompt)
        };
        Ok(SubagentResult {
            success: true,
            output: Arc::from(output),
            subagent_id: request.id.clone(),
            child_session_id: request.id,
            tokens_used: 7,
            ..Default::default()
        })
    }

    async fn query(
        &self,
        _id: &str,
        _block: bool,
        _timeout_ms: Option<u64>,
    ) -> Option<SubagentSnapshot> {
        None
    }

    async fn cancel(&self, id: &str) -> SubagentCancelOutcome {
        self.cancelled_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id.to_string());
        if self
            .cancel_not_found_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return SubagentCancelOutcome::NotFound;
        }
        if self.block_spawns.load(Ordering::SeqCst) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        self.cancellation_observed.add_permits(1);
        SubagentCancelOutcome::Cancelled
    }

    async fn validate_type(
        &self,
        subagent_type: &str,
        _parent_session_id: &str,
    ) -> SubagentValidateTypeOutcome {
        if subagent_type == "explore" {
            SubagentValidateTypeOutcome::Ok
        } else {
            SubagentValidateTypeOutcome::Unknown {
                available: vec!["explore".to_string()],
            }
        }
    }

    async fn describe_subagent_type(
        &self,
        subagent_type: &str,
        _harness_agent_type: Option<&str>,
        _parent_session_id: &str,
    ) -> SubagentDescribeOutcome {
        if subagent_type == "explore" {
            SubagentDescribeOutcome::Ok(SubagentTypeSummary {
                can_read: true,
                can_search: true,
                ..Default::default()
            })
        } else {
            SubagentDescribeOutcome::Unavailable
        }
    }
}

#[tokio::test]
async fn cancellation_retries_the_spawn_registration_window() {
    let backend = Arc::new(FakeBackend::default());
    backend
        .cancel_not_found_remaining
        .store(2, Ordering::SeqCst);
    let resource = SubagentBackendResource(backend.clone());

    assert!(cancel_worker(&resource, "queued-child").await);
    assert_eq!(
        backend
            .cancelled_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        3
    );
}

fn resources(backend: Arc<FakeBackend>, session_folder: &Path, cwd: &Path) -> Resources {
    let mut resources = Resources::new();
    resources.insert(SubagentBackendResource(backend));
    resources.insert(SessionIdResource("parent-session".to_string()));
    resources.insert(CurrentPromptIdResource("parent-prompt".to_string()));
    resources.insert(TaskModelValidator::new(|_| None));
    resources.insert(SessionFolder(session_folder.to_path_buf()));
    resources.insert(Cwd(cwd.to_path_buf()));
    resources
}

async fn run(
    backend: Arc<FakeBackend>,
    session_folder: &Path,
    input: WorkflowInput,
) -> Result<Value, xai_tool_runtime::ToolError> {
    let output = xai_tool_runtime::Tool::run(
        &WorkflowTool,
        test_ctx(resources(backend, session_folder, session_folder).into_shared()),
        input,
    )
    .await?;
    match output {
        ToolOutput::Text(text) => serde_json::from_str(&text.text).map_err(|error| {
            xai_tool_runtime::ToolError::custom(
                "test_output",
                format!("workflow returned invalid JSON text: {error}"),
            )
        }),
        other => panic!("unexpected workflow output: {other:?}"),
    }
}

fn input(script: &str) -> WorkflowInput {
    WorkflowInput {
        script: script.to_string(),
        saved_workflow: None,
        args: json!({}),
        resume_from_run_id: None,
        max_concurrency: Some(2),
        timeout_seconds: Some(10),
        max_agents: None,
        max_tokens: None,
        retention_days: None,
        run_in_background: false,
        approval_hash: None,
    }
}

async fn wait_for_single_run_dir(session_folder: &Path) -> PathBuf {
    for _ in 0..200 {
        if let Ok(entries) = fs::read_dir(session_folder.join("workflows")) {
            let run_dirs = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect::<Vec<_>>();
            if run_dirs.len() == 1 {
                return run_dirs.into_iter().next().unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("workflow run directory was not created");
}

async fn wait_for_run_unlock(run_dir: &Path) {
    for _ in 0..200 {
        if let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(run_dir.join("run.lock"))
            && fs2::FileExt::try_lock_exclusive(&file).is_ok()
        {
            fs2::FileExt::unlock(&file).unwrap();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("workflow run lock was not released");
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_and_pipeline_are_bounded_ordered_and_read_only() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "scheduler-test", description: "exercise both primitives" };
phase("Fan out");
const first = await parallel([
  () => agent("parallel:a", { label: "parallel:a" }),
  () => agent("parallel:b", { label: "parallel:b" }),
  () => agent("parallel:c", { label: "parallel:c" })
]);
phase("Pipeline");
const second = await pipeline(
  ["a", "b", "c"],
  (item) => agent(`inspect:${item}`, {
    label: `inspect:${item}`,
    schema: {
      type: "object",
      required: ["item", "ok"],
      properties: {
        item: { type: "string" },
        ok: { type: "boolean" }
      }
    }
  }),
  (inspection, original) => agent(
    `verify:${original}:${inspection.item}`,
    { label: `verify:${original}` }
  )
);
return { first, second };
"#;

    let summary = run(backend.clone(), temp.path(), input(script))
        .await
        .unwrap();
    assert_eq!(summary["status"], "completed");
    assert_eq!(summary["agents_spawned"], 9);
    assert_eq!(summary["agents_completed"], 9);
    assert_eq!(summary["max_active_agents"], 2);
    assert_eq!(
        summary["result"]["first"],
        json!([
            "result:parallel:a",
            "result:parallel:b",
            "result:parallel:c"
        ])
    );
    assert_eq!(
        summary["result"]["second"],
        json!([
            "result:verify:a:a",
            "result:verify:b:b",
            "result:verify:c:c"
        ])
    );
    assert_eq!(summary["phases"], json!(["Fan out", "Pipeline"]));
    assert_eq!(backend.max_active.load(Ordering::SeqCst), 2);

    let requests = backend.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests.len(), 9);
    assert!(requests.iter().all(|request| request.run_in_background));
    assert!(requests.iter().all(|request| !request.surface_completion));
    assert!(requests.iter().all(|request| !request.fork_context));
    assert!(requests.iter().all(|request| request.strict_read_only));
    assert!(
        requests
            .iter()
            .all(|request| { request.capability_mode == Some(SubagentCapabilityMode::ReadOnly) })
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.json_schema.is_some())
            .count(),
        3
    );
    assert!(requests.iter().any(|request| request.prompt == "inspect:a"));
}

#[tokio::test(flavor = "multi_thread")]
async fn pipeline_starts_the_next_stage_without_a_global_barrier() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "pipeline-flow", description: "prove per-item streaming" };
return await pipeline(
  ["slow", "fast"],
  (item) => agent(`stage1:${item}`, { label: `stage1:${item}` }),
  (_value, item) => agent(`stage2:${item}`, { label: `stage2:${item}` })
);
"#;

    let summary = run(backend.clone(), temp.path(), input(script))
        .await
        .unwrap();
    assert_eq!(
        summary["result"],
        json!(["result:stage2:slow", "result:stage2:fast"])
    );

    let prompts = backend
        .requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|request| request.prompt.clone())
        .collect::<Vec<_>>();
    let fast_stage_two = prompts
        .iter()
        .position(|prompt| prompt == "stage2:fast")
        .unwrap();
    let slow_stage_two = prompts
        .iter()
        .position(|prompt| prompt == "stage2:slow")
        .unwrap();
    assert!(
        fast_stage_two < slow_stage_two,
        "the fast item should advance while the slow item remains in stage one: {prompts:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_reuses_completed_labelled_workers() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "resume-test", description: "cache completed workers" };
return await parallel([
  () => agent("one", { label: "one" }),
  () => agent("two", { label: "two" })
]);
"#;
    let first = run(backend.clone(), temp.path(), input(script))
        .await
        .unwrap();
    assert_eq!(backend.spawn_count.load(Ordering::SeqCst), 2);
    let run_id = first["run_id"].as_str().unwrap().to_string();

    let mut resumed_input = input(script);
    resumed_input.resume_from_run_id = Some(run_id);
    let second = run(backend.clone(), temp.path(), resumed_input)
        .await
        .unwrap();
    assert_eq!(second["agents_spawned"], 0);
    assert_eq!(second["cache_hits"], 2);
    assert_eq!(second["result"], first["result"]);
    assert_eq!(backend.spawn_count.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_run_reuses_only_completed_labelled_workers() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "interrupted-resume", description: "reuse durable prefix work" };
const first = await agent("arm-next", { label: "first" });
const second = await agent("blocked-second", { label: "second" });
return [first, second];
"#;
    let ctx = test_ctx(resources(backend.clone(), temp.path(), temp.path()).into_shared());
    let running = tokio::spawn(async move {
        xai_tool_runtime::Tool::run(&WorkflowTool, ctx, input(script)).await
    });

    tokio::time::timeout(Duration::from_secs(2), backend.spawn_started.acquire())
        .await
        .expect("second worker should start")
        .expect("start semaphore should stay open")
        .forget();
    let run_dir = wait_for_single_run_dir(temp.path()).await;
    let run_id = run_dir.file_name().unwrap().to_string_lossy().into_owned();

    running.abort();
    let _ = running.await;
    tokio::time::timeout(
        Duration::from_secs(2),
        backend.cancellation_observed.acquire(),
    )
    .await
    .expect("interrupted worker should be cancelled")
    .expect("cancellation semaphore should stay open")
    .forget();
    wait_for_run_unlock(&run_dir).await;

    backend.block_spawns.store(false, Ordering::SeqCst);
    let mut resumed = input(script);
    resumed.resume_from_run_id = Some(run_id);
    let summary = run(backend.clone(), temp.path(), resumed).await.unwrap();
    assert_eq!(summary["status"], "completed");
    assert_eq!(summary["cache_hits"], 1);
    assert_eq!(summary["agents_spawned"], 1);
    assert_eq!(
        summary["result"],
        json!(["result:arm-next", "result:blocked-second"])
    );
    assert_eq!(backend.spawn_count.load(Ordering::SeqCst), 3);

    let journal = fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    assert_eq!(journal.matches("\"event\":\"cache_hit\"").count(), 1);
    assert_eq!(journal.matches("\"event\":\"agent_completed\"").count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_active_run_cannot_be_resumed_concurrently() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    backend.block_spawns.store(true, Ordering::SeqCst);
    let script = r#"
export const meta = { name: "resume-lock", description: "one owner per run" };
return await agent("blocked", { label: "blocked" });
"#;
    let folder = temp.path().to_path_buf();
    let first_backend = backend.clone();
    let first = tokio::spawn(async move { run(first_backend, &folder, input(script)).await });
    tokio::time::timeout(Duration::from_secs(2), backend.spawn_started.acquire())
        .await
        .expect("first worker should start")
        .expect("start semaphore should stay open")
        .forget();

    let run_id = fs::read_dir(temp.path().join("workflows"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    let mut resumed = input(script);
    resumed.resume_from_run_id = Some(run_id);
    let error = run(backend.clone(), temp.path(), resumed)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("already active"), "{error}");

    first.abort();
    let _ = first.await;
    tokio::time::timeout(
        Duration::from_secs(2),
        backend.cancellation_observed.acquire(),
    )
    .await
    .expect("aborted owner should cancel its worker")
    .expect("cancellation semaphore should stay open")
    .forget();
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_failure_is_partial_and_does_not_abort_siblings() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "partial-test", description: "keep unrelated work" };
return await parallel([
  () => agent("good:a", { label: "a" }),
  () => agent("intentional-failure", { label: "bad" }),
  () => agent("good:c", { label: "c" })
]);
"#;
    let summary = run(backend, temp.path(), input(script)).await.unwrap();
    assert_eq!(summary["status"], "partial");
    assert_eq!(summary["agents_completed"], 2);
    assert_eq!(summary["agents_failed"], 1);
    assert_eq!(
        summary["result"],
        json!(["result:good:a", null, "result:good:c"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_output_is_revalidated_before_completion_or_caching() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "schema-mismatch", description: "fail closed on backend mismatch" };
return await agent("wrong-shape", {
  label: "wrong-shape",
  schema: {
    type: "object",
    additionalProperties: false,
    required: ["item", "ok"],
    properties: {
      item: { type: "string" },
      ok: { type: "boolean" }
    }
  }
});
"#;

    let summary = run(backend.clone(), temp.path(), input(script))
        .await
        .unwrap();
    assert_eq!(summary["status"], "partial");
    assert_eq!(summary["agents_completed"], 0);
    assert_eq!(summary["agents_failed"], 1);
    assert_eq!(summary["result"], Value::Null);

    let run_dir = Path::new(summary["run_dir"].as_str().unwrap());
    assert!(
        fs::read_dir(run_dir.join("results"))
            .unwrap()
            .next()
            .is_none(),
        "non-conforming output must not enter the replay cache"
    );
    let journal = fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    assert!(journal.contains("does not match the required schema"));
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_schema_omits_unique_items_while_local_validation_keeps_it() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "schema-dialect", description: "project provider schemas safely" };
return await agent("inspect:a", {
  label: "schema-dialect",
  schema: {
    type: "object",
    additionalProperties: false,
    required: ["item", "ok"],
    properties: {
      item: { type: "string" },
      ok: { type: "boolean" },
      tags: {
        type: "array",
        uniqueItems: true,
        items: {
          type: "array",
          uniqueItems: true,
          items: { type: "string" }
        }
      },
      uniqueItems: {
        type: "array",
        uniqueItems: true,
        items: { type: "string" }
      },
      payload: {
        const: { uniqueItems: true }
      }
    }
  }
});
"#;

    let summary = run(backend.clone(), temp.path(), input(script))
        .await
        .unwrap();
    assert_eq!(summary["status"], "completed");

    let requests = backend.requests.lock().unwrap_or_else(|e| e.into_inner());
    let provider_schema = requests[0].json_schema.as_ref().unwrap();
    assert!(
        provider_schema
            .pointer("/properties/tags/uniqueItems")
            .is_none()
    );
    assert!(
        provider_schema
            .pointer("/properties/tags/items/uniqueItems")
            .is_none()
    );
    assert!(provider_schema.pointer("/properties/uniqueItems").is_some());
    assert!(
        provider_schema
            .pointer("/properties/uniqueItems/uniqueItems")
            .is_none()
    );
    assert_eq!(
        provider_schema.pointer("/properties/payload/const"),
        Some(&json!({ "uniqueItems": true }))
    );

    let full_schema = json!({
        "type": "array",
        "uniqueItems": true,
        "items": { "type": "string" }
    });
    let projected = provider_compatible_schema(&full_schema);
    assert_eq!(full_schema["uniqueItems"], true);
    assert!(projected.get("uniqueItems").is_none());
    assert!(
        jsonschema::validator_for(&full_schema)
            .unwrap()
            .validate(&json!(["duplicate", "duplicate"]))
            .is_err()
    );
    assert!(
        jsonschema::validator_for(&projected)
            .unwrap()
            .validate(&json!(["duplicate", "duplicate"]))
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_and_cache_records_are_size_bounded() {
    let oversized_temp = tempfile::tempdir().unwrap();
    let oversized_backend = Arc::new(FakeBackend::default());
    let oversized_script = r#"
export const meta = { name: "oversized-worker", description: "drop oversized output" };
return await agent("oversized-output", { label: "oversized" });
"#;
    let oversized = run(
        oversized_backend,
        oversized_temp.path(),
        input(oversized_script),
    )
    .await
    .unwrap();
    assert_eq!(oversized["status"], "partial");
    assert_eq!(oversized["result"], Value::Null);
    assert_eq!(oversized["agents_failed"], 1);
    let oversized_run_dir = Path::new(oversized["run_dir"].as_str().unwrap());
    assert!(
        fs::read_dir(oversized_run_dir.join("results"))
            .unwrap()
            .next()
            .is_none()
    );

    let boundary_temp = tempfile::tempdir().unwrap();
    let boundary_backend = Arc::new(FakeBackend::default());
    let boundary_script = r#"
export const meta = { name: "boundary-worker", description: "replay a near-limit value" };
return await agent("boundary-output", { label: "boundary" });
"#;
    let first = run(
        boundary_backend.clone(),
        boundary_temp.path(),
        input(boundary_script),
    )
    .await
    .unwrap();
    assert_eq!(first["status"], "completed");
    assert_eq!(
        first["result"].as_str().unwrap().len(),
        MAX_RESULT_BYTES - 2
    );
    let mut resumed = input(boundary_script);
    resumed.resume_from_run_id = Some(first["run_id"].as_str().unwrap().to_string());
    let replayed = run(boundary_backend.clone(), boundary_temp.path(), resumed)
        .await
        .unwrap();
    assert_eq!(replayed["cache_hits"], 1);
    assert_eq!(replayed["agents_spawned"], 0);
    assert_eq!(boundary_backend.spawn_count.load(Ordering::SeqCst), 1);

    let tampered_temp = tempfile::tempdir().unwrap();
    let tampered_backend = Arc::new(FakeBackend::default());
    let tampered_script = r#"
export const meta = { name: "tampered-cache", description: "ignore oversized cache records" };
return await agent("normal-output", { label: "tampered" });
"#;
    let first = run(
        tampered_backend.clone(),
        tampered_temp.path(),
        input(tampered_script),
    )
    .await
    .unwrap();
    let run_dir = Path::new(first["run_dir"].as_str().unwrap());
    let cache_path = run_dir
        .join("results")
        .join(format!("{}.json", stable_hash(b"tampered")));
    fs::write(&cache_path, vec![b'x'; MAX_CACHE_RECORD_BYTES + 1]).unwrap();

    let mut resumed = input(tampered_script);
    resumed.resume_from_run_id = Some(first["run_id"].as_str().unwrap().to_string());
    let replayed = run(tampered_backend.clone(), tampered_temp.path(), resumed)
        .await
        .unwrap();
    assert_eq!(replayed["cache_hits"], 0);
    assert_eq!(replayed["agents_spawned"], 1);
    assert_eq!(tampered_backend.spawn_count.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_diagnostics_are_truncated_before_journaling() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "bounded-errors", description: "bound backend diagnostics" };
return await agent("oversized-error", { label: "error" });
"#;
    let summary = run(backend, temp.path(), input(script)).await.unwrap();
    assert_eq!(summary["status"], "partial");
    let journal =
        fs::read_to_string(Path::new(summary["run_dir"].as_str().unwrap()).join("journal.jsonl"))
            .unwrap();
    assert!(journal.contains("[truncated]"));
    assert!(journal.len() < MAX_DIAGNOSTIC_BYTES * 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn javascript_failures_are_truncated_before_persistence_and_return() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "bounded-runtime-error", description: "bound thrown values" };
throw "e".repeat(1024 * 1024);
"#;
    let error = run(backend, temp.path(), input(script))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("[truncated]"));
    assert!(error.len() < MAX_DIAGNOSTIC_BYTES + 1024);

    let run_dir = wait_for_single_run_dir(temp.path()).await;
    let persisted: Value = read_json(&run_dir.join("result.json")).unwrap();
    let persisted_error = persisted["error"].as_str().unwrap();
    assert!(persisted_error.contains("[truncated]"));
    assert!(persisted_error.len() <= MAX_DIAGNOSTIC_BYTES);
    let journal = fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    assert!(journal.len() < MAX_DIAGNOSTIC_BYTES * 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_has_no_ambient_host_capabilities_and_args_are_frozen() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let mut workflow = input(
        r#"
export const meta = { name: "sandbox-test", description: "inspect globals" };
let mutationBlocked = false;
try {
  args.value = 99;
} catch (_) {
  mutationBlocked = true;
}
return {
  process: typeof process,
  require: typeof require,
  date: typeof Date,
  performance: typeof performance,
  webAssembly: typeof WebAssembly,
  nativeAgent: typeof __nativeAgent,
  nativePhase: typeof __nativePhase,
  mutationBlocked,
  randomBlocked: (() => {
    try { Math.random(); return false; } catch (_) { return true; }
  })(),
  value: args.value
};
"#,
    );
    workflow.args = json!({ "value": 7 });
    let summary = run(backend, temp.path(), workflow).await.unwrap();
    assert_eq!(
        summary["result"],
        json!({
            "process": "undefined",
            "require": "undefined",
            "date": "undefined",
            "performance": "undefined",
            "webAssembly": "undefined",
            "nativeAgent": "undefined",
            "nativePhase": "undefined",
            "mutationBlocked": true,
            "randomBlocked": true,
            "value": 7
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unresolved_javascript_promises_obey_deadline_and_cancellation() {
    let script = r#"
export const meta = { name: "unresolved-promise", description: "exercise host bounds" };
return await new Promise(() => {});
"#;

    let deadline_temp = tempfile::tempdir().unwrap();
    let mut deadline_input = input(script);
    deadline_input.timeout_seconds = Some(1);
    let deadline_error = tokio::time::timeout(
        Duration::from_secs(3),
        run(
            Arc::new(FakeBackend::default()),
            deadline_temp.path(),
            deadline_input,
        ),
    )
    .await
    .expect("pure JavaScript promise should obey the workflow deadline")
    .unwrap_err()
    .to_string();
    assert!(
        deadline_error.contains("deadline exceeded"),
        "{deadline_error}"
    );

    let cancellation_temp = tempfile::tempdir().unwrap();
    let cancellation = CancellationToken::new();
    let mut ctx = test_ctx(
        resources(
            Arc::new(FakeBackend::default()),
            cancellation_temp.path(),
            cancellation_temp.path(),
        )
        .into_shared(),
    );
    ctx.insert(xai_tool_runtime::Cancellation(cancellation.clone()));
    let running = tokio::spawn(async move {
        xai_tool_runtime::Tool::run(&WorkflowTool, ctx, input(script)).await
    });
    let run_dir = wait_for_single_run_dir(cancellation_temp.path()).await;
    cancellation.cancel();
    let cancellation_error = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("external cancellation should stop an unresolved JavaScript promise")
        .expect("workflow task should not panic")
        .unwrap_err()
        .to_string();
    assert!(
        cancellation_error.contains("cancelled"),
        "{cancellation_error}"
    );
    wait_for_run_unlock(&run_dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_an_unresolved_javascript_run_releases_its_lock() {
    let temp = tempfile::tempdir().unwrap();
    let script = r#"
export const meta = { name: "dropped-promise", description: "stop with the caller" };
return await new Promise(() => {});
"#;
    let ctx = test_ctx(
        resources(Arc::new(FakeBackend::default()), temp.path(), temp.path()).into_shared(),
    );
    let running = tokio::spawn(async move {
        xai_tool_runtime::Tool::run(&WorkflowTool, ctx, input(script)).await
    });
    let run_dir = wait_for_single_run_dir(temp.path()).await;

    running.abort();
    let _ = running.await;
    wait_for_run_unlock(&run_dir).await;
    let persisted: Value = read_json(&run_dir.join("result.json")).unwrap();
    assert_eq!(persisted["status"], "failed");
    assert!(
        persisted["error"]
            .as_str()
            .is_some_and(|error| error.contains("cancelled"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_losing_agent_promise_cannot_leave_an_orphan() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    backend.block_spawns.store(true, Ordering::SeqCst);
    let script = r#"
export const meta = { name: "promise-race", description: "reject unfinished losing branches" };
return await Promise.race([
  agent("blocked-loser", { label: "loser" }),
  Promise.resolve("winner")
]);
"#;

    let error = tokio::time::timeout(
        Duration::from_secs(3),
        run(backend.clone(), temp.path(), input(script)),
    )
    .await
    .expect("unfinished agent guard should finish promptly")
    .unwrap_err()
    .to_string();
    assert!(error.contains("unfinished agent call"), "{error}");
    assert_eq!(backend.active.load(Ordering::SeqCst), 0);
    if backend.spawn_count.load(Ordering::SeqCst) > 0 {
        assert!(
            !backend
                .cancelled_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }
    let run_dir = wait_for_single_run_dir(temp.path()).await;
    wait_for_run_unlock(&run_dir).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn phase_events_are_deduplicated_and_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "phase-limit", description: "bound native progress state" };
for (let index = 0; index < 1000; index++) {
  phase(`phase:${index % 2}`);
}
return true;
"#;

    let summary = run(backend, temp.path(), input(script)).await.unwrap();
    assert_eq!(
        summary["phases"].as_array().unwrap().len(),
        MAX_PHASE_EVENTS
    );
    let run_dir = summary["run_dir"].as_str().unwrap();
    let journal = fs::read_to_string(Path::new(run_dir).join("journal.jsonl")).unwrap();
    assert_eq!(
        journal.matches("\"event\":\"phase\"").count(),
        MAX_PHASE_EVENTS
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_script_and_duplicate_labels_fail_before_extra_spawns() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let invalid = run(backend.clone(), temp.path(), input("return 1;"))
        .await
        .unwrap_err()
        .to_string();
    assert!(invalid.contains("export const meta"));
    assert_eq!(backend.spawn_count.load(Ordering::SeqCst), 0);

    let spoofed = run(
        backend.clone(),
        temp.path(),
        input("// export const meta = {};\nreturn 1;"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(spoofed.contains("must begin with literal metadata"));
    assert_eq!(backend.spawn_count.load(Ordering::SeqCst), 0);

    let executable_metadata = run(
        backend.clone(),
        temp.path(),
        input("export const meta = await agent('hidden'); return null;"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(executable_metadata.contains("object literal"));
    assert_eq!(backend.spawn_count.load(Ordering::SeqCst), 0);

    let inert_metadata = run(
        backend.clone(),
        temp.path(),
        input(
            r#"
export const meta = {
  name: await agent("must-not-run"),
  description: "metadata is erased before evaluation"
};
return 7;
"#,
        ),
    )
    .await
    .unwrap();
    assert_eq!(inert_metadata["result"], 7);
    assert_eq!(backend.spawn_count.load(Ordering::SeqCst), 0);

    let duplicate = input(
        r#"
export const meta = { name: "duplicate-test", description: "reject ambiguous replay" };
await agent("first", { label: "same" });
return await agent("second", { label: "same" });
"#,
    );
    let error = run(backend.clone(), temp.path(), duplicate)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicated"));
    assert_eq!(backend.spawn_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn schemas_are_self_contained_and_cannot_resolve_files_or_urls() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let cases = [
        (
            r#"export const meta = { name: "http-ref", description: "reject network" };
return await agent("x", { schema: { "$ref": "https://example.com/schema.json" } });"#,
            "local fragment",
        ),
        (
            r#"export const meta = { name: "file-ref", description: "reject files" };
return await agent("x", { schema: { "allOf": [{ "$ref": "file:///etc/passwd" }] } });"#,
            "local fragment",
        ),
        (
            r#"export const meta = { name: "base-id", description: "reject bases" };
return await agent("x", { schema: { "$id": "https://example.com/base", "type": "object" } });"#,
            "$id",
        ),
        (
            r#"export const meta = { name: "meta-schema", description: "reject schema lookup" };
return await agent("x", { schema: { "$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object" } });"#,
            "$schema",
        ),
    ];

    for (script, expected) in cases {
        let error = run(backend.clone(), temp.path(), input(script))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
    }
    assert_eq!(backend.spawn_count.load(Ordering::SeqCst), 0);

    let local = r##"
export const meta = { name: "local-ref", description: "allow local definitions" };
return await agent("inspect:local", {
  schema: {
    type: "object",
    properties: { item: { "$ref": "#/$defs/item" } },
    "$defs": { item: { type: "string" } }
  }
});
"##;
    let summary = run(backend.clone(), temp.path(), input(local))
        .await
        .unwrap();
    assert_eq!(summary["status"], "completed");
    assert_eq!(backend.spawn_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn invocation_budget_is_enforced_before_async_workers_are_polled() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "admission-limit", description: "bound queued native futures" };
const calls = [];
for (let index = 0; index < 1000; index++) {
  calls.push(agent(`work:${index}`, { label: `work:${index}` }));
}
return await Promise.all(calls);
"#;

    let error = run(backend.clone(), temp.path(), input(script))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("16-agent"), "{error}");
    assert!(backend.spawn_count.load(Ordering::SeqCst) <= MAX_AGENTS);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_stops_every_active_worker() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    backend.block_spawns.store(true, Ordering::SeqCst);
    let cancellation = CancellationToken::new();
    let script = r#"
export const meta = { name: "cancel-test", description: "cancel active workers" };
return await parallel([
  () => agent("blocked:a", { label: "a" }),
  () => agent("blocked:b", { label: "b" })
]);
"#;
    let mut ctx = test_ctx(resources(backend.clone(), temp.path(), temp.path()).into_shared());
    ctx.insert(xai_tool_runtime::Cancellation(cancellation.clone()));

    let running = tokio::spawn(async move {
        xai_tool_runtime::Tool::run(&WorkflowTool, ctx, input(script)).await
    });
    tokio::time::timeout(
        Duration::from_secs(2),
        backend.spawn_started.acquire_many(2),
    )
    .await
    .expect("both workers should start")
    .expect("start semaphore should stay open")
    .forget();

    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .expect("workflow cancellation should finish promptly")
        .expect("workflow task should not panic")
        .expect_err("cancelled workflow should fail")
        .to_string();
    assert!(error.contains("cancelled"), "{error}");

    let mut cancelled = backend
        .cancelled_ids
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    cancelled.sort();
    assert_eq!(cancelled.len(), 2);
    assert_eq!(backend.active.load(Ordering::SeqCst), 0);

    let run_dir = fs::read_dir(temp.path().join("workflows"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let result: Value = read_json(&run_dir.join("result.json")).unwrap();
    assert_eq!(result["status"], "failed");
    let journal = fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    assert_eq!(journal.matches("\"event\":\"agent_cancelled\"").count(), 2);
    assert!(journal.contains("\"event\":\"failed\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_tool_future_cancels_workers_without_runtime_context() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    backend.block_spawns.store(true, Ordering::SeqCst);
    let script = r#"
export const meta = { name: "drop-test", description: "hard cancellation path" };
return await parallel([
  () => agent("blocked:a", { label: "a" }),
  () => agent("blocked:b", { label: "b" })
]);
"#;
    let ctx = test_ctx(resources(backend.clone(), temp.path(), temp.path()).into_shared());
    let running = tokio::spawn(async move {
        xai_tool_runtime::Tool::run(&WorkflowTool, ctx, input(script)).await
    });
    tokio::time::timeout(
        Duration::from_secs(2),
        backend.spawn_started.acquire_many(2),
    )
    .await
    .expect("both workers should start")
    .expect("start semaphore should stay open")
    .forget();

    running.abort();
    let _ = running.await;
    tokio::time::timeout(
        Duration::from_secs(2),
        backend.cancellation_observed.acquire_many(2),
    )
    .await
    .expect("drop guard should cancel both workers")
    .expect("cancellation semaphore should stay open")
    .forget();
    assert_eq!(backend.active.load(Ordering::SeqCst), 0);
}

#[test]
fn approval_hash_binds_arguments_and_limits() {
    let script =
        "export const meta = { name: \"hash\", description: \"hash\" }; return args.value;";
    let mut base = input(script);
    base.args = json!({"value": 1});
    let first = workflow_approval_hash(&base).unwrap();
    base.args = json!({"value": 2});
    assert_ne!(first, workflow_approval_hash(&base).unwrap());
    base.args = json!({"value": 1});
    base.max_agents = Some(3);
    assert_ne!(first, workflow_approval_hash(&base).unwrap());
}

#[test]
fn saved_workflow_resolves_from_project_and_rejects_traversal() {
    let temp = tempfile::tempdir().unwrap();
    let workflows = temp.path().join(".atlas/workflows");
    fs::create_dir_all(&workflows).unwrap();
    let source = "export const meta = { name: \"saved\", description: \"saved\" }; return 1;";
    fs::write(workflows.join("audit.js"), source).unwrap();

    let mut saved = input("");
    saved.saved_workflow = Some("audit".to_string());
    let path = resolve_workflow_source(&mut saved, temp.path())
        .unwrap()
        .unwrap();
    assert_eq!(path, workflows.join("audit.js"));
    assert_eq!(saved.script, source);

    let mut traversal = input("");
    traversal.saved_workflow = Some("../audit".to_string());
    assert!(resolve_workflow_source(&mut traversal, temp.path()).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn write_worker_requests_fail_closed_worktree_profile() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let script = r#"
export const meta = { name: "write", description: "write worker profile" };
return await agent("edit the requested file", {
  label: "writer",
  mode: "write",
  isolation: "worktree"
});
"#;
    run(backend.clone(), temp.path(), input(script))
        .await
        .unwrap();
    let requests = backend
        .requests
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].strict_read_only);
    assert!(requests[0].strict_workflow_write);
    assert_eq!(
        requests[0].capability_mode,
        Some(SubagentCapabilityMode::ReadWrite)
    );
    assert_eq!(
        requests[0].isolation,
        Some(xai_tool_types::SubagentIsolationMode::Worktree)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn production_preview_approval_and_supervisor_path_execute() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(FakeBackend::default());
    let cancellation = CancellationToken::new();
    let handle = supervisor::WorkflowSupervisor::start(
        temp.path().join("workflows"),
        crate::notification::types::ToolNotificationHandle::noop(),
        cancellation.clone(),
    );
    let mut test_resources = resources(backend, temp.path(), temp.path());
    test_resources.insert(handle.clone());
    let shared = test_resources.into_shared();
    let script = "export const meta = { name: \"production\", description: \"supervised\" }; return { ok: true };";

    let missing_approval =
        xai_tool_runtime::Tool::run(&WorkflowTool, test_ctx(shared.clone()), input(script))
            .await
            .unwrap_err()
            .to_string();
    assert!(missing_approval.contains("workflow_preview"));

    let mut approved_input = input(script);
    approved_input.run_in_background = true;
    let preview = xai_tool_runtime::Tool::run(
        &controls::WorkflowPreviewTool,
        test_ctx(shared.clone()),
        approved_input.clone(),
    )
    .await
    .unwrap();
    let ToolOutput::Text(preview) = preview else {
        panic!("preview should return text");
    };
    let preview: Value = serde_json::from_str(&preview.text).unwrap();
    approved_input.approval_hash = Some(
        preview["approval_hash"]
            .as_str()
            .expect("preview approval hash")
            .to_string(),
    );

    let started = xai_tool_runtime::Tool::run(&WorkflowTool, test_ctx(shared), approved_input)
        .await
        .unwrap();
    let ToolOutput::Text(started) = started else {
        panic!("workflow should return text");
    };
    let started: Value = serde_json::from_str(&started.text).unwrap();
    let run_id = started["runId"].as_str().unwrap().to_string();

    for _ in 0..100 {
        let snapshot = supervisor::get(&handle, run_id.clone())
            .await
            .unwrap()
            .unwrap();
        if snapshot.status.is_terminal() {
            assert_eq!(snapshot.status, supervisor::WorkflowRunStatus::Completed);
            cancellation.cancel();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("supervised workflow did not complete");
}
