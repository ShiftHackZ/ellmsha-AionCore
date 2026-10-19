//! Exercise the real adapter's reader and outbound wire, not a capabilities mock.
//! Response/override shapes verified from the bundled Codex 0.162.1 generated
//! v2/{ThreadStart,ThreadResume,ThreadFork}{Params,Response} and TurnStartParams.
use super::*;
use crate::testing::FakeAgentIo;

#[test]
fn required_model_takes_priority_over_generation_preference_on_all_open_methods() {
    let mut config = SessionConfig {
        required_model: Some("model-a".into()),
        model: Some("model-b".into()),
        generation_only: true,
        ..Default::default()
    };
    for HandshakeParams(params) in [
        thread_start_params(&config),
        thread_resume_params(&config, "thread"),
        thread_fork_params(&config, "thread", None),
    ] {
        assert_eq!(params["model"], "model-a");
    }
    config.required_model = None;
    assert_eq!(thread_start_params(&config).0["model"], "model-b");
}

async fn fixture(
    required: Option<&str>,
    observed: Option<&str>,
    mode: HandshakeMode<'_>,
) -> (CodexSessionBackend, Arc<Mutex<Vec<u8>>>, Box<dyn Fn() + Send + Sync>) {
    let mut result = json!({"thread":{"id":"thread"}});
    if let Some(model) = observed {
        result["model"] = json!(model);
    }
    let response = format!(
        "{}\n{}\n",
        json!({"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"thread"}}}),
        json!({"jsonrpc":"2.0","id":2,"result":result}),
    );
    let drift = json!({"jsonrpc":"2.0","method":"thread/settings/updated",
        "params":{"threadId":"thread","threadSettings":{"model":"model-b"}}});
    let fake = FakeAgentIo::new(Vec::new(), None)
        .with_gated_segments(vec![response.into_bytes(), format!("{drift}\n").into_bytes()]);
    let release: Box<dyn Fn() + Send + Sync> = Box::new(fake.segment_releaser());
    let captured = fake.captured_stdin();
    let mut backend = CodexSessionBackend::build_with_io("routed-pin", Box::new(fake)).await;
    backend.capabilities.current_model = Some("model-a".into());
    *backend.wake.config.write().unwrap() = SessionConfig {
        model: Some("model-a".into()),
        required_model: required.map(str::to_owned),
        ..Default::default()
    };
    backend.run_handshake(mode).await.unwrap();
    (backend, captured, release)
}

fn send() -> Command {
    Command::Send {
        content: vec![ContentBlock::Text("synthetic fixture".into())],
        metadata: Default::default(),
    }
}

async fn frames(captured: &Mutex<Vec<u8>>) -> Vec<Value> {
    String::from_utf8(captured.lock().await.clone())
        .unwrap()
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn wait_frame(captured: &Mutex<Vec<u8>>, method: &str) -> Vec<Value> {
    for _ in 0..100 {
        let wire = frames(captured).await;
        if wire.iter().any(|frame| frame["method"] == method) {
            return wire;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("outbound {method} was not captured");
}

#[tokio::test]
async fn frozen_resume_rejects_changed_or_missing_wire_model_before_turn() {
    for observed in [Some("model-b"), None] {
        let (backend, captured, release) = fixture(Some("model-a"), observed, HandshakeMode::Resume("thread")).await;
        assert!(
            backend
                .bound_thread_within(std::time::Duration::from_millis(50))
                .await
                .is_err(),
            "optimistic model-a seed and thread binding must not admit the turn"
        );
        release();
        let error = backend
            .bound_thread_within(std::time::Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(error, BackendError::SetupRejected(ref reason) if reason == ROUTED_MODEL_MISMATCH));
        assert!(matches!(
            backend.dispatch(send()).await,
            Err(BackendError::SetupRejected(_))
        ));
        let wire = frames(&captured).await;
        assert_eq!(
            wire.iter().find(|v| v["method"] == "thread/resume").unwrap()["params"]["model"],
            "model-a"
        );
        assert!(wire.iter().all(|v| v["method"] != "turn/start"));
        assert!(!backend.turn_in_flight.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn frozen_resume_confirms_and_enforces_model_on_turn_then_blocks_drift() {
    let (backend, captured, release) = fixture(Some("model-a"), Some("model-a"), HandshakeMode::Resume("thread")).await;
    release();
    backend
        .bound_thread_within(std::time::Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(backend.capabilities().current_model.as_deref(), Some("model-a"));
    backend.dispatch(send()).await.unwrap();
    assert!(
        matches!(backend.dispatch(Command::SetModel { model: "model-b".into() }).await,
        Err(BackendError::SetupRejected(ref reason)) if reason == ROUTED_MODEL_MISMATCH)
    );
    let wire = wait_frame(&captured, "turn/start").await;
    assert_eq!(wire.iter().filter(|v| v["method"] == "turn/start").count(), 1);
    assert_eq!(
        wire.iter().find(|v| v["method"] == "turn/start").unwrap()["params"]["model"],
        "model-a"
    );
    assert!(wire.iter().all(|v| v["method"] != "thread/settings/update"));
    release();
    let error = backend.bound_thread_within(std::time::Duration::from_secs(1)).await;
    // The reader may not have consumed the gated notification on the first poll.
    if error.is_ok() {
        for _ in 0..100 {
            if backend.resume_poison.lock().await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    assert!(matches!(
        backend.dispatch(send()).await,
        Err(BackendError::SetupRejected(_))
    ));
    assert_eq!(
        frames(&captured)
            .await
            .iter()
            .filter(|v| v["method"] == "turn/start")
            .count(),
        1
    );
}

#[tokio::test]
async fn frozen_fresh_and_fork_require_actual_response_model() {
    for mode in [
        HandshakeMode::Fresh,
        HandshakeMode::Fork {
            parent_thread_id: "parent",
            last_turn_id: None,
        },
    ] {
        let (backend, captured, release) = fixture(Some("model-a"), Some("model-b"), mode).await;
        release();
        assert!(
            matches!(backend.bound_thread_within(std::time::Duration::from_secs(1)).await,
            Err(BackendError::SetupRejected(ref reason)) if reason == ROUTED_MODEL_MISMATCH)
        );
        let wire = frames(&captured).await;
        let open = wire
            .iter()
            .find(|v| v["method"] == "thread/start" || v["method"] == "thread/fork")
            .unwrap();
        assert_eq!(open["params"]["model"], "model-a");
        assert!(wire.iter().all(|v| v["method"] != "turn/start"));
    }
}

#[tokio::test]
async fn frozen_fresh_notification_cannot_replace_open_response_confirmation() {
    let fake = FakeAgentIo::new(Vec::new(), None).with_gated_segments(vec![
        br#"{"jsonrpc":"2.0","method":"thread/started","params":{"thread":{"id":"thread"}}}
{"jsonrpc":"2.0","method":"thread/settings/updated","params":{"threadId":"thread","threadSettings":{"model":"model-a"}}}
"#
        .to_vec(),
        br#"{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"thread"},"model":"model-b"}}
"#
        .to_vec(),
    ]);
    let release = fake.segment_releaser();
    let backend = CodexSessionBackend::build_with_io("routed-open-gate", Box::new(fake)).await;
    backend.wake.config.write().unwrap().required_model = Some("model-a".into());
    backend.run_handshake(HandshakeMode::Fresh).await.unwrap();
    release();
    assert!(matches!(
        backend.bound_thread_within(std::time::Duration::from_millis(100)).await,
        Err(BackendError::HandshakeTimeout(_))
    ));
    release();
    assert!(
        matches!(backend.bound_thread_within(std::time::Duration::from_secs(1)).await,
        Err(BackendError::SetupRejected(ref reason)) if reason == ROUTED_MODEL_MISMATCH)
    );
}

#[tokio::test]
async fn ordinary_and_v1_resume_preserve_unpinned_wire_and_model_switching() {
    let (backend, captured, release) = fixture(None, Some("model-b"), HandshakeMode::Resume("thread")).await;
    release();
    backend
        .bound_thread_within(std::time::Duration::from_secs(1))
        .await
        .unwrap();
    backend.dispatch(send()).await.unwrap();
    backend
        .dispatch(Command::SetModel {
            model: "model-c".into(),
        })
        .await
        .unwrap();
    let wire = wait_frame(&captured, "thread/settings/update").await;
    for method in ["thread/resume", "turn/start"] {
        assert!(
            wire.iter().find(|v| v["method"] == method).unwrap()["params"]
                .get("model")
                .is_none()
        );
    }
    assert_eq!(
        wire.iter().find(|v| v["method"] == "thread/settings/update").unwrap()["params"]["model"],
        "model-c"
    );
}
