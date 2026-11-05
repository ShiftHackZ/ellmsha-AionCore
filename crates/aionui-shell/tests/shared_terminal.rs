//! Real PTY tests: no renderer or provider fixture substitutes for a terminal.
use aionui_api_types::{
    StartBackgroundProcessRequest, TerminalInputRequest, TerminalResizeRequest, TerminalWaitRequest,
};
use aionui_db::{IConversationRepository, SqliteConversationRepository, init_database_memory};
use aionui_shell::BackgroundProcessService;
use std::sync::Arc;
const OWNER: &str = "system_default_user";
async fn fixture() -> (BackgroundProcessService, tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    for id in ["chat", "other"] {
        let row = serde_json::from_value(serde_json::json!({"id":id,"user_id":OWNER,"name":"PTY fixture","type":"acp","extra":serde_json::json!({"workspace":dir.path(),"is_temporary_workspace":true}).to_string(),"pinned":false,"status":"finished","source":"aionui","created_at":0,"updated_at":0})).unwrap();
        repo.create(&row).await.unwrap();
    }
    let service = BackgroundProcessService::new(repo);
    let shell = service
        .start(
            OWNER,
            "chat",
            StartBackgroundProcessRequest {
                command: String::new(),
                title: None,
                cwd: None,
            },
        )
        .await
        .unwrap();
    (service, dir, shell.id)
}
fn input(data: &str, revision: Option<u64>) -> TerminalInputRequest {
    TerminalInputRequest {
        data: data.into(),
        expected_revision: revision,
    }
}
async fn wait(service: &BackgroundProcessService, id: &str, text: &str) -> aionui_api_types::BackgroundProcessSnapshot {
    let read = service
        .wait(
            OWNER,
            "chat",
            id,
            TerminalWaitRequest {
                text: text.into(),
                timeout_ms: 5000,
            },
        )
        .await
        .unwrap();
    assert!(read.stdout.contains(text), "missing {text}: {}", read.stdout);
    read
}
#[cfg(unix)]
#[tokio::test]
async fn user_and_agent_share_tty_cwd_input_and_screen_even_without_a_renderer() {
    let (service, dir, id) = fixture().await;
    service
        .write(
            OWNER,
            "chat",
            &id,
            input("test -t 0 && test -t 1 && printf 'REAL_%s\\n' PTY\r", Some(0)),
            true,
        )
        .await
        .unwrap();
    let snap = wait(&service, &id, "REAL_PTY").await;
    service
        .write(
            OWNER,
            "chat",
            &id,
            input("mkdir child; cd child; printf 'USER_%s\\n' CWD\r", None),
            false,
        )
        .await
        .unwrap();
    let snap2 = wait(&service, &id, "USER_CWD").await;
    assert_eq!(snap2.input_revision, snap.input_revision + 1);
    assert!(
        service
            .write(OWNER, "chat", &id, input("echo bad\r", Some(snap.input_revision)), true)
            .await
            .is_err()
    );
    assert!(
        service
            .write(
                OWNER,
                "chat",
                &id,
                input("echo bad\r", Some(snap2.input_revision)),
                true
            )
            .await
            .is_err(),
        "human lease must hold even with a fresh revision"
    );
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    service
        .write(OWNER, "chat", &id, input("pwd\r", Some(snap2.input_revision)), true)
        .await
        .unwrap();
    wait(&service, &id, &dir.path().join("child").to_string_lossy()).await;
    assert!(
        service
            .write(OWNER, "other", &id, input("bad", None), false)
            .await
            .is_err()
    );
    service.shutdown().await;
}
#[cfg(unix)]
#[tokio::test]
async fn resize_fullscreen_binary_output_reconnect_and_ctrl_c_work() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let (service, _dir, id) = fixture().await;
    service
        .resize(OWNER, "chat", &id, TerminalResizeRequest { rows: 32, cols: 101 })
        .await
        .unwrap();
    service
        .write(
            OWNER,
            "chat",
            &id,
            input(
                "stty size; printf '\\033[2J\\033[H%s%s' 'SCREEN' '_OK'; sleep 30\r",
                Some(0),
            ),
            true,
        )
        .await
        .unwrap();
    let snap = wait(&service, &id, "SCREEN_OK").await;
    assert_eq!((snap.rows, snap.cols), (32, 101));
    let chunk = service.output(OWNER, "chat", &id, Some(0), None).await.unwrap();
    assert!(!STANDARD.decode(&chunk.data).unwrap().is_empty());
    let delta = service
        .output(OWNER, "chat", &id, Some(chunk.next_offset), Some(chunk.screen_revision))
        .await
        .unwrap();
    assert!(!delta.reset);
    let restored = service.output(OWNER, "chat", &id, Some(u64::MAX), None).await.unwrap();
    assert!(restored.reset);
    let mut emulator = vt100::Parser::new(32, 101, 0);
    emulator.process(&STANDARD.decode(restored.data).unwrap());
    assert!(emulator.screen().contents().contains("SCREEN_OK"));
    service
        .write(OWNER, "chat", &id, input("\x03", Some(snap.input_revision)), true)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let revision = service.read(OWNER, "chat", &id).await.unwrap().input_revision;
    service
        .write(
            OWNER,
            "chat",
            &id,
            input("printf 'AFTER_%s\\n' INTERRUPT\r", Some(revision)),
            true,
        )
        .await
        .unwrap();
    wait(&service, &id, "AFTER_INTERRUPT").await;
    assert!(
        service
            .resize(OWNER, "chat", &id, TerminalResizeRequest { rows: 0, cols: 90 })
            .await
            .is_err()
    );
    assert!(service.output(OWNER, "other", &id, None, None).await.is_err());
    assert!(
        service
            .wait(
                OWNER,
                "chat",
                &id,
                TerminalWaitRequest {
                    text: String::new(),
                    timeout_ms: 10001
                }
            )
            .await
            .is_err()
    );
    service.shutdown().await;
}
#[cfg(unix)]
#[tokio::test]
async fn screen_keeps_up_with_redraw_and_output_overflow_without_splitting_utf8() {
    let (service, _dir, id) = fixture().await;
    service
        .write(
            OWNER,
            "chat",
            &id,
            input(
                "head -c 300000 /dev/zero; printf '\\033[2J\\033[H%s%s' '你好_' 'ТЕРМІНАЛ'; sleep 30\r",
                Some(0),
            ),
            true,
        )
        .await
        .unwrap();
    let screen = wait(&service, &id, "你好_ТЕРМІНАЛ").await;
    assert!(screen.stdout_truncated);
    let recovery = service.output(OWNER, "chat", &id, Some(0), None).await.unwrap();
    assert!(recovery.reset);
    service.shutdown().await;
}

#[tokio::test]
async fn native_shell_accepts_input_and_shutdown_closes_its_pty_on_every_os() {
    let (service, _dir, id) = fixture().await;
    #[cfg(windows)]
    let data = "echo NATIVE_^SHELL\r";
    #[cfg(not(windows))]
    let data = "printf 'NATIVE_%s\\n' SHELL\r";
    service
        .write(OWNER, "chat", &id, input(data, Some(0)), true)
        .await
        .unwrap();
    let marker = "NATIVE_SHELL";
    let snap = wait(&service, &id, marker).await;
    service.shutdown().await;
    assert!(service.list(OWNER, "chat").await.unwrap().is_empty());
    assert!(matches!(
        aionui_process::probe(snap.pid),
        aionui_process::ObservedLiveness::Gone
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn watch_runs_on_the_real_terminal_and_can_be_interrupted() {
    let watch = ["/usr/bin/watch", "/opt/homebrew/bin/watch"]
        .iter()
        .find(|path| std::path::Path::new(path).exists());
    let Some(watch) = watch else {
        return;
    };
    let (service, _dir, id) = fixture().await;
    let command = format!("{watch} -t -n 0.2 \"printf 'WATCH_%s' WORKS\"\r");
    service
        .write(OWNER, "chat", &id, input(&command, Some(0)), true)
        .await
        .unwrap();
    let snap = wait(&service, &id, "WATCH_WORKS").await;
    assert!(!snap.stdout.contains("Inappropriate ioctl"));
    service
        .write(OWNER, "chat", &id, input("\x03", Some(snap.input_revision)), true)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let snap = service.read(OWNER, "chat", &id).await.unwrap();
    assert!(snap.running, "CtrlC must leave the shell alive");
    service.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn close_marks_all_views_closed_and_keeps_final_screen() {
    let (service, _dir, id) = fixture().await;
    service
        .write(
            OWNER,
            "chat",
            &id,
            input("printf 'FINAL_%s\\n' SCREEN\r", Some(0)),
            true,
        )
        .await
        .unwrap();
    wait(&service, &id, "FINAL_SCREEN").await;
    let closed = service.close(OWNER, "chat", &id).await.unwrap();
    assert!(closed.closed && !closed.running);
    assert!(closed.stdout.contains("FINAL_SCREEN"));
    assert!(service.list(OWNER, "chat").await.unwrap()[0].closed);
    assert!(service.close(OWNER, "chat", &id).await.unwrap().closed);
    service.shutdown().await;
}
#[cfg(unix)]
#[tokio::test]
async fn a_raw_program_that_does_not_read_stdin_cannot_block_close_or_reuse_an_input_revision() {
    let (service, _dir, id) = fixture().await;
    service
        .write(
            OWNER,
            "chat",
            &id,
            input("stty raw -echo; printf 'RAW_%s' READY; sleep 30\r", Some(0)),
            true,
        )
        .await
        .unwrap();
    let ready = wait(&service, &id, "RAW_READY").await;
    let data = "x".repeat(65536);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        service.write(OWNER, "chat", &id, input(&data, Some(ready.input_revision)), true),
    )
    .await;
    assert!(result.is_ok(), "PTY input backpressure must be bounded");
    let next = service.read(OWNER, "chat", &id).await.unwrap();
    assert_eq!(next.input_revision, ready.input_revision + 1);
    tokio::time::timeout(std::time::Duration::from_secs(5), service.close(OWNER, "chat", &id))
        .await
        .unwrap()
        .unwrap();
    service.shutdown().await;
}
