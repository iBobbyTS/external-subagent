//! Full-chain model-selection acceptance tests.
//!
//! These drive the real daemon scheduler against the fake ZCode runtime
//! binary in this crate, so the bootstrap's two-step switch (session/create
//! catalog → session/setModel → provider-qualified read-back) is exercised on
//! the wire exactly as production would. `ZCODE_FAKE_LOG` records every frame
//! the runtime received, which is what the wire assertions read.

use external_daemon::{CommandRuntimeFactory, Scheduler, SchedulerConfig, SchedulerError};
use external_store::{Store, TaskOutcome};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn fixture_bin() -> &'static str {
    env!("CARGO_BIN_EXE_external-fixture-zcode")
}

fn admission(model: Option<&str>, effort: Option<&str>) -> external_core::AdmissionIdentity {
    external_core::AdmissionIdentity {
        agent: "zcode".into(),
        config_revision: 1,
        adapter_version: "model-selection-test".into(),
        model: model.map(str::to_owned),
        model_source: "catalog".into(),
        effort: effort.map(str::to_owned),
    }
}

fn manifest(workspace: &Path) -> external_core::GeneralTaskManifest {
    external_core::GeneralTaskManifest {
        schema: external_core::GENERAL_TASK_SCHEMA.into(),
        agent_id: String::new(),
        repository: workspace.canonicalize().unwrap(),
        permission_mode: external_core::PermissionMode::Plan,
        prompt: "auto_complete".into(),
        write_manifest: Vec::new(),
    }
}

struct Case {
    _directory: tempfile::TempDir,
    scheduler: Scheduler,
    log: PathBuf,
    agent_id: String,
    start: Result<Vec<String>, SchedulerError>,
}

impl Case {
    fn outcome(&self) -> TaskOutcome {
        await_result(&self.scheduler, &self.agent_id).result.outcome
    }

    fn reason_code(&self) -> Option<String> {
        self.scheduler
            .store()
            .terminal_reason_code(&self.agent_id)
            .unwrap()
    }

    fn last_error(&self) -> Option<String> {
        self.scheduler.last_error(&self.agent_id)
    }

    fn frames(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn set_model_frame(&self) -> Option<Value> {
        self.frames()
            .into_iter()
            .find(|frame| frame["method"] == "session/setModel")
    }

    fn assert_no_set_model(&self) {
        assert!(
            self.set_model_frame().is_none(),
            "no session/setModel frame may be emitted: {:?}",
            self.frames()
        );
    }
}

fn run_case(
    prefix: &str,
    envs: &[(&'static str, &'static str)],
    model: Option<&str>,
    effort: Option<&str>,
) -> Case {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/live-agent/workspace");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&root)
        .unwrap();
    let log = directory.path().join("wire.jsonl");
    let cwd = directory.path().to_path_buf();
    let log_for_factory = log.clone();
    let envs = envs.to_vec();
    let factory = CommandRuntimeFactory::new(move |_: &external_store::TaskRecord| {
        let mut command = Command::new(fixture_bin());
        command.arg("session").current_dir(&cwd);
        command.env("ZCODE_FAKE_LOG", &log_for_factory);
        for (name, value) in &envs {
            command.env(name, value);
        }
        Ok(command)
    });
    let store = Arc::new(Store::open(directory.path().join("state.sqlite")).unwrap());
    let scheduler = Scheduler::new(
        "zcode-model-selection-test",
        store,
        Arc::new(factory),
        SchedulerConfig {
            bootstrap_timeout: Duration::from_secs(30),
            control_timeout: Duration::from_secs(30),
            ..SchedulerConfig::default()
        },
    )
    .unwrap();
    let submitted = scheduler
        .enqueue_general_with_admission(&manifest(directory.path()), Some(admission(model, effort)))
        .unwrap();
    let start = scheduler.start_ready();
    Case {
        _directory: directory,
        scheduler,
        log,
        agent_id: submitted.agent_id,
        start,
    }
}

fn await_result(scheduler: &Scheduler, agent_id: &str) -> external_store::StoredTaskResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = scheduler.store().task_result(agent_id).unwrap() {
            return result;
        }
        assert!(Instant::now() < deadline, "no terminal result");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn set_model_params(case: &Case) -> Value {
    case.set_model_frame().expect("setModel frame recorded")["params"].clone()
}

/// AC①: admitted effort resolves to a catalog reasoning level and the wire
/// carries the provider-qualified model plus persistAsWorkspaceLastUsed=false.
#[test]
fn ac1_admitted_effort_reaches_set_model_with_a_resolved_level() {
    let case = run_case("s02-ac1-effort-", &[], Some("zai/GLM-5.3"), Some("high"));
    assert_eq!(case.start.as_ref().unwrap(), &vec![case.agent_id.clone()]);
    assert_eq!(case.outcome(), TaskOutcome::Completed);
    assert_eq!(
        set_model_params(&case),
        json!({
            "sessionId": "fake-session-7f3a",
            "model": {
                "providerId": "zai",
                "modelId": "GLM-5.3",
                "options": {"reasoningLevel": "high"}
            },
            "persistAsWorkspaceLastUsed": false
        })
    );
}

/// AC①: with no admitted effort the catalog entry's defaultLevel is applied.
#[test]
fn ac1_default_level_is_used_when_no_effort_is_admitted() {
    let case = run_case("s02-ac1-default-", &[], Some("zai/GLM-5.3"), None);
    assert_eq!(case.start.as_ref().unwrap(), &vec![case.agent_id.clone()]);
    assert_eq!(case.outcome(), TaskOutcome::Completed);
    assert_eq!(
        set_model_params(&case)["model"]["options"],
        json!({"reasoningLevel": "max"})
    );
}

/// AC①: no effort and no defaultLevel omits the whole options key.
#[test]
fn ac1_options_are_omitted_when_no_level_resolves() {
    let catalog = r#"[{"ref":{"providerId":"zai","modelId":"GLM-5.3"},"reasoning":{"levels":[{"value":"low","label":"low"}]}}]"#;
    let case = run_case(
        "s02-ac1-nolevel-",
        &[("ZCODE_FAKE_MODEL_CATALOG", catalog)],
        Some("zai/GLM-5.3"),
        None,
    );
    assert_eq!(case.start.as_ref().unwrap(), &vec![case.agent_id.clone()]);
    assert_eq!(case.outcome(), TaskOutcome::Completed);
    let params = set_model_params(&case);
    assert!(
        params["model"].get("options").is_none(),
        "no resolved level must omit options: {params}"
    );
}

/// AC①b: a non-`zai` provider round-trips provider-qualified, so a bare
/// `zai/<token>` fallback can never produce a false MODEL_MISMATCH.
#[test]
fn ac1b_non_zai_provider_round_trips_provider_qualified() {
    let case = run_case(
        "s02-ac1b-deepseek-",
        &[],
        Some("deepseek/deepseek-flash"),
        Some("low"),
    );
    assert_eq!(case.start.as_ref().unwrap(), &vec![case.agent_id.clone()]);
    assert_eq!(case.outcome(), TaskOutcome::Completed);
    assert_eq!(
        set_model_params(&case)["model"],
        json!({
            "providerId": "deepseek",
            "modelId": "deepseek-flash",
            "options": {"reasoningLevel": "low"}
        })
    );
}

/// AC②: a token outside the create catalog fails closed with bounded
/// available evidence and no setModel frame; the scheduler keeps classifying
/// an InvalidSession bootstrap failure as SESSION_START_FAILED.
#[test]
fn ac2_unoffered_model_fails_closed_before_set_model() {
    let case = run_case("s02-ac2-unoffered-", &[], Some("zai/NOT-A-MODEL"), None);
    assert!(
        case.start.is_err(),
        "unoffered model must fail the bootstrap"
    );
    assert_eq!(case.outcome(), TaskOutcome::Failed);
    assert_eq!(case.reason_code().as_deref(), Some("SESSION_START_FAILED"));
    let failure = case.last_error().unwrap();
    assert!(
        failure.contains("MODEL_NOT_OFFERED") && failure.contains("available:"),
        "failure must name the missing offer and its bounded evidence: {failure}"
    );
    assert!(failure.contains("zai/GLM-5.3"), "evidence list: {failure}");
    case.assert_no_set_model();
}

/// AC③: documented remote rejections are discriminated by error.data.code
/// only and surface as ModelRejected (scheduler code MODEL_REJECTED).
#[test]
fn ac3_remote_model_rejection_maps_to_model_rejected() {
    for code in ["model_not_found", "invalid_model_request"] {
        let case = run_case(
            "s02-ac3-remote-",
            &[("ZCODE_FAKE_SETMODEL_ERROR_CODE", code)],
            Some("zai/GLM-5.3"),
            Some("high"),
        );
        assert!(case.start.is_err(), "{code} must fail the bootstrap");
        assert_eq!(case.outcome(), TaskOutcome::Failed);
        assert_eq!(
            case.reason_code().as_deref(),
            Some("MODEL_REJECTED"),
            "remote rejection of {code} must classify as MODEL_REJECTED"
        );
        let failure = case.last_error().unwrap();
        assert!(
            failure.contains("model selection was rejected") && failure.contains(code),
            "failure must carry the discriminating code {code}: {failure}"
        );
    }
}

/// AC④: a success result whose read-back diverges fails closed as
/// MODEL_MISMATCH even though the request was offered.
#[test]
fn ac4_diverging_readback_fails_closed() {
    let case = run_case(
        "s02-ac4-mismatch-",
        &[(
            "ZCODE_FAKE_SETMODEL_CURRENT",
            r#"{"providerId":"zai","modelId":"GLM-5.3-Flash"}"#,
        )],
        Some("zai/GLM-5.3"),
        Some("high"),
    );
    assert!(case.start.is_err(), "diverging read-back must fail");
    assert_eq!(case.outcome(), TaskOutcome::Failed);
    assert_eq!(case.reason_code().as_deref(), Some("SESSION_START_FAILED"));
    assert!(
        case.last_error().unwrap().contains("MODEL_MISMATCH"),
        "failure must name MODEL_MISMATCH: {:?}",
        case.last_error()
    );
}

/// AC⑤: with no admitted model the historical effort bootstrap is unchanged:
/// no setModel frame, the create thoughtLevel passes and the task completes.
#[test]
fn ac5_requested_none_keeps_the_legacy_bootstrap() {
    let case = run_case(
        "s02-ac5-none-",
        &[("ZCODE_FAKE_EFFORT_ECHO", "high")],
        None,
        Some("high"),
    );
    assert_eq!(case.start.as_ref().unwrap(), &vec![case.agent_id.clone()]);
    assert_eq!(case.outcome(), TaskOutcome::Completed);
    let frames = case.frames();
    let create = frames
        .iter()
        .find(|frame| frame["method"] == "session/create")
        .expect("create frame");
    assert_eq!(create["params"]["thoughtLevel"], "high");
    case.assert_no_set_model();
}

/// AC⑥: the model_changed state.updated notification arrives mid-request and
/// must not disturb the pump or the following turn.
#[test]
fn ac6_state_updated_notification_does_not_disturb_the_pump() {
    let case = run_case("s02-ac6-notify-", &[], Some("zai/GLM-5.3"), Some("high"));
    assert_eq!(case.start.as_ref().unwrap(), &vec![case.agent_id.clone()]);
    assert_eq!(
        case.outcome(),
        TaskOutcome::Completed,
        "the interleaved state.updated must not break the turn"
    );
    assert_eq!(case.reason_code(), None);
    assert!(
        case.frames()
            .iter()
            .any(|frame| frame["method"] == "session/send"),
        "the confirmed session must still take its prompt"
    );
}

/// AC⑦: an admitted effort outside the catalog entry's levels is rejected
/// before any setModel frame, with the levels as evidence.
#[test]
fn ac7_effort_outside_catalog_levels_is_rejected_before_set_model() {
    let case = run_case("s02-ac7-effort-", &[], Some("zai/GLM-5.3"), Some("medium"));
    assert!(case.start.is_err(), "unavailable effort must fail early");
    assert_eq!(case.outcome(), TaskOutcome::Failed);
    assert_eq!(case.reason_code().as_deref(), Some("SESSION_START_FAILED"));
    let failure = case.last_error().unwrap();
    assert!(
        failure.contains("MODEL_NOT_OFFERED")
            && failure.contains("medium")
            && failure.contains("low, high, max"),
        "failure must carry the level evidence: {failure}"
    );
    case.assert_no_set_model();
}
