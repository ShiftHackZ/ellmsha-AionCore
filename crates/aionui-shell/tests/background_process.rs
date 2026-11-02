use aionui_api_types::StartBackgroundProcessRequest;
use aionui_db::{IConversationRepository, SqliteConversationRepository, init_database_memory};
use aionui_shell::{BackgroundProcessService, background_process::BackgroundProcessError};
use std::sync::Arc;
use std::time::Duration;

const OWNER: &str = "system_default_user";
async fn fixture() -> (BackgroundProcessService, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    for id in ["first", "second"] {
        let row = serde_json::from_value(serde_json::json!({
            "id":id,"user_id":OWNER,"name":"Fixture","type":"acp",
            "extra":serde_json::json!({"workspace":directory.path()}).to_string(),
            "pinned":false,"status":"finished","source":"aionui","created_at":0,"updated_at":0
        }))
        .unwrap();
        repo.create(&row).await.unwrap();
    }
    (BackgroundProcessService::new(repo), directory)
}
fn command(text: &str) -> StartBackgroundProcessRequest {
    StartBackgroundProcessRequest {
        command: text.into(),
        title: Some("Fixture server".into()),
        cwd: None,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn captures_output_and_stops_the_owned_command_without_losing_its_terminal() {
    let (service, _directory) = fixture().await;
    let started = service
        .start(
            OWNER,
            "first",
            command("printf 'ready'; printf 'diagnostic' >&2; sleep 30"),
        )
        .await
        .unwrap();
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = service.read(OWNER, "first", &started.id).await.unwrap();
            if snapshot.stdout.contains("readydiagnostic") {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(snapshot.running);
    assert!(
        service.list(OWNER, "first").await.unwrap()[0].stdout.is_empty(),
        "inventory must not copy output buffers"
    );
    let stopped = service.stop(OWNER, "first", &started.id).await.unwrap();
    assert!(!stopped.running);
    assert!(stopped.stdout.contains("readydiagnostic"));
    assert!(
        !service.stop(OWNER, "first", &started.id).await.unwrap().running,
        "Stop is idempotent"
    );
    service.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn stop_reaps_a_server_that_ignores_term_after_its_shell_exits() {
    let (service, _directory) = fixture().await;
    let started = service.start(OWNER, "first", command("sh -c 'trap \"\" TERM; echo $$ > child.pid; exec sleep 60' & while [ ! -s child.pid ]; do sleep 0.01; done; cat child.pid; wait")).await.unwrap();
    let child = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = service.read(OWNER, "first", &started.id).await.unwrap();
            if let Ok(pid) = snapshot.stdout.trim().parse::<u32>() {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    // Give the synthetic child's TERM trap time to install before Stop.
    tokio::time::sleep(Duration::from_millis(100)).await;
    service.stop(OWNER, "first", &started.id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(aionui_process::probe(child), aionui_process::ObservedLiveness::Gone) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("owned descendant must be gone");
    service.shutdown().await;
}

#[tokio::test]
async fn ownership_invalid_commands_and_cwd_escape_fail_closed() {
    let (service, _directory) = fixture().await;
    assert!(matches!(
        service.start("foreign", "first", command("echo forbidden")).await,
        Err(BackgroundProcessError::NotFound)
    ));
    assert!(matches!(
        service.start(OWNER, "first", command("bad\0command")).await,
        Err(BackgroundProcessError::Invalid(_))
    ));
    let mut escaped = command("echo forbidden");
    escaped.cwd = Some("..".into());
    assert!(matches!(
        service.start(OWNER, "first", escaped).await,
        Err(BackgroundProcessError::Invalid(
            "Working directory must be within the conversation workspace"
        ))
    ));
    assert!(matches!(
        service.read(OWNER, "second", "arbitrary-pid-123").await,
        Err(BackgroundProcessError::NotFound)
    ));
    service.shutdown().await;
    assert!(matches!(
        service.start(OWNER, "first", command("echo forbidden")).await,
        Err(BackgroundProcessError::ShuttingDown)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_stops_running_commands_and_cross_conversation_reads_and_stops_are_rejected() {
    let (service, _directory) = fixture().await;
    let started = service.start(OWNER, "first", command("sleep 30")).await.unwrap();
    assert!(matches!(
        service.read("foreign", "first", &started.id).await,
        Err(BackgroundProcessError::NotFound)
    ));
    assert!(matches!(
        service.stop(OWNER, "second", &started.id).await,
        Err(BackgroundProcessError::NotFound)
    ));
    service.shutdown().await;
    assert!(matches!(
        aionui_process::probe(started.pid),
        aionui_process::ObservedLiveness::Gone
    ));
    assert!(service.list(OWNER, "first").await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn output_is_bounded_and_fast_exit_retains_final_stderr() {
    let (service, _directory) = fixture().await;
    let started = service
        .start(
            OWNER,
            "first",
            command("head -c 400000 /dev/zero; printf 'final-error' >&2"),
        )
        .await
        .unwrap();
    let done = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = service.read(OWNER, "first", &started.id).await.unwrap();
            if !snapshot.running && snapshot.stdout_truncated && snapshot.stdout.contains("final-error") {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(done.stdout.len() <= 256 * 1024);
    assert!(done.stderr.is_empty());
    service.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn owned_terminal_remains_stoppable_after_its_workspace_is_removed() {
    let (service, directory) = fixture().await;
    let started = service.start(OWNER, "first", command("sleep 30")).await.unwrap();
    directory.close().unwrap();
    assert_eq!(service.list(OWNER, "first").await.unwrap().len(), 1);
    assert!(service.read(OWNER, "first", &started.id).await.unwrap().running);
    assert!(!service.stop(OWNER, "first", &started.id).await.unwrap().running);
    service.shutdown().await;
}
