use super::*;
use aionui_session::{Admission, BackendError, Capabilities, CommandReceipt};
use futures_util::{StreamExt, stream::BoxStream};
use std::sync::Mutex;

struct ObservedBackend {
    model: Mutex<Option<String>>,
    dispatches: AtomicI64,
    terminations: AtomicI64,
}
#[async_trait::async_trait]
impl SessionBackend for ObservedBackend {
    async fn dispatch(&self, _: Command) -> Result<CommandReceipt, BackendError> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        Ok(CommandReceipt {
            accepted: true,
            admission: Admission::NoTurn,
            turn_gen: 0,
        })
    }
    async fn terminate(&self) {
        self.terminations.fetch_add(1, Ordering::SeqCst);
    }
    fn events(&self) -> BoxStream<'static, SessionEnvelope> {
        futures_util::stream::pending().boxed()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            current_model: self.model.lock().unwrap().clone(),
            ..Default::default()
        }
    }
}
fn fixture() -> (Arc<SessionAgentTask>, Arc<ObservedBackend>) {
    let backend = Arc::new(ObservedBackend {
        model: Mutex::new(Some("frozen".into())),
        dispatches: AtomicI64::new(0),
        terminations: AtomicI64::new(0),
    });
    let task = SessionAgentTask::new(
        AgentType::Acp,
        "chat".into(),
        "user".into(),
        "/tmp".into(),
        backend.clone(),
        None,
    );
    (task, backend)
}
fn message() -> SendMessageData {
    SendMessageData {
        content: "fixture task".into(),
        msg_id: "m".into(),
        turn_id: None,
        files: vec![],
        inject_skills: vec![],
    }
}

#[tokio::test]
async fn routed_session_blocks_switch_and_live_drift_before_dispatch() {
    let (task, backend) = fixture();
    task.require_routed_model("frozen".into()).await.unwrap();
    assert!(matches!(task.set_config_option("model", "different").await,
        Err(AgentError::BadRequest(reason)) if reason == crate::routing::guard::PIN_ERROR));
    assert_eq!(backend.dispatches.load(Ordering::SeqCst), 0);
    task.send_message(message()).await.unwrap();
    assert_eq!(backend.dispatches.load(Ordering::SeqCst), 1);
    *backend.model.lock().unwrap() = Some("different".into());
    assert_eq!(
        task.send_message(message())
            .await
            .unwrap_err()
            .stream_error()
            .detail
            .as_deref(),
        Some(crate::routing::guard::PIN_ERROR)
    );
    assert_eq!(backend.dispatches.load(Ordering::SeqCst), 1);
    assert!(
        task.require_routed_model("different".into()).await.is_err(),
        "pin is immutable"
    );
}

#[tokio::test]
async fn ordinary_session_switching_is_unchanged() {
    let (task, backend) = fixture();
    task.set_config_option("model", "different").await.unwrap();
    task.send_message(message()).await.unwrap();
    assert_eq!(backend.dispatches.load(Ordering::SeqCst), 2);
    assert_eq!(
        task.get_model()
            .await
            .unwrap()
            .model_info
            .unwrap()
            .current_model_id
            .as_deref(),
        Some("different")
    );
}

#[tokio::test]
async fn routed_generation_uses_pin_and_never_prior_catalog_selection() {
    let backend = Arc::new(ObservedBackend {
        model: Mutex::new(None),
        dispatches: AtomicI64::new(0),
        terminations: AtomicI64::new(0),
    });
    let task = SessionAgentTask::build(
        AgentType::Acp,
        "chat".into(),
        "user".into(),
        "/tmp".into(),
        backend.clone(),
        None,
        CatalogPreload {
            current_model: Some("stale-preload".into()),
            ..Default::default()
        },
        None,
        None,
    );
    task.require_routed_model("frozen".into()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let spawner = Arc::new(aionui_process::RealSpawner::new(
        Arc::new(aionui_process::FileRegistryStore::new(dir.path())),
        uuid::Uuid::now_v7(),
        "fixture-machine",
    ));
    *task.generation.lock().unwrap() = Some(crate::generation::GenerationTarget::Cli {
        backend: "codex".into(),
        config: Box::new(aionui_session::SessionConfig {
            model: Some("frozen".into()),
            ..Default::default()
        }),
        spawner,
    });
    assert_eq!(task.generation_target().unwrap().model(), "frozen");
    *backend.model.lock().unwrap() = Some("drift".into());
    assert!(
        matches!(task.generation_target(), Err(AgentError::BadRequest(reason)) if reason == crate::routing::guard::PIN_ERROR)
    );
    assert_eq!(backend.dispatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn routed_session_initial_mismatch_terminates_open_backend_before_exposure() {
    let (task, backend) = fixture();
    assert!(matches!(task.require_routed_model("different".into()).await,
        Err(AgentError::BadRequest(reason)) if reason == crate::routing::guard::PIN_ERROR));
    assert_eq!(backend.terminations.load(Ordering::SeqCst), 1);
    assert_eq!(backend.dispatches.load(Ordering::SeqCst), 0);
}
