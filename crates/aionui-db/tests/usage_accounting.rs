use aionui_db::{SqliteUsageRepository, UsageQuery, UsageReport, init_database_memory};
use serde_json::{Value, json};

async fn setup() -> (aionui_db::Database, SqliteUsageRepository) {
    let db = init_database_memory().await.unwrap();
    for user in ["alice", "bob"] {
        sqlx::query("INSERT INTO users(id,username,password_hash,created_at,updated_at) VALUES(?,?,?,0,0)")
            .bind(user)
            .bind(user)
            .bind("fixture")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,model,created_at,updated_at) VALUES(?,?,?,'aionrs','{}',?,0,0)").bind(user).bind(user).bind(user).bind(r#"{"provider_id":"provider-a","model":"model-a"}"#).execute(db.pool()).await.unwrap();
    }
    let repo = SqliteUsageRepository::new(db.pool().clone());
    (db, repo)
}
fn report(id: &str) -> UsageReport {
    UsageReport {
        request_id: id.into(),
        counter: None,
        billing_reported: true,
        context_reported: true,
        provider_id: None,
        is_child: false,
        model: "model-a".into(),
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 40,
        cache_creation_tokens: 10,
        context_tokens: 120,
        context_window: 8192,
        request_metrics: Default::default(),
        breakdown: json!({"tools":30,"messages":90}),
    }
}
async fn totals(repo: &SqliteUsageRepository, user: &str, q: UsageQuery) -> Value {
    serde_json::to_value(repo.query(user, &q).await.unwrap()).unwrap()
}

#[tokio::test]
async fn repeated_delivery_is_not_double_counted_and_cache_remains_an_input_subset() {
    let (_db, repo) = setup().await;
    let r = report("one");
    repo.record("alice", "alice", &r).await.unwrap();
    repo.record("alice", "alice", &r).await.unwrap();
    let d = totals(&repo, "alice", UsageQuery::default()).await;
    assert_eq!(
        d["totals"],
        json!({"input_tokens":100,"output_tokens":20,"cache_read_tokens":40,"cache_creation_tokens":10,"requests":1})
    );
}
#[tokio::test]
async fn ownership_is_enforced_on_both_reads_and_writes() {
    let (_db, repo) = setup().await;
    repo.record("alice", "bob", &report("foreign")).await.unwrap();
    repo.record("bob", "bob", &report("own")).await.unwrap();
    assert_eq!(
        totals(&repo, "alice", UsageQuery::default()).await["totals"]["requests"],
        0
    );
    assert_eq!(
        totals(&repo, "bob", UsageQuery::default()).await["totals"]["requests"],
        1
    );
}
#[tokio::test]
async fn period_provider_model_and_conversation_filters_intersect() {
    let (db, repo) = setup().await;
    repo.record("alice", "alice", &report("early")).await.unwrap();
    let mut later = report("later");
    later.model = "model-b".into();
    repo.record("alice", "alice", &later).await.unwrap();
    sqlx::query("UPDATE usage_requests SET created_at=CASE request_id WHEN 'early' THEN 1000 ELSE 2000 END")
        .execute(db.pool())
        .await
        .unwrap();
    let q = UsageQuery {
        days: None,
        start: Some(2000),
        end: Some(3000),
        conversation_id: Some("alice".into()),
        provider: Some("provider-a".into()),
        model: Some("model-b".into()),
    };
    let d = totals(&repo, "alice", q).await;
    assert_eq!(d["totals"]["requests"], 1);
    assert_eq!(d["series"][0]["model"], "model-b");
}
#[tokio::test]
async fn latest_context_is_a_snapshot_while_input_output_accumulate() {
    let (_db, repo) = setup().await;
    repo.record("alice", "alice", &report("one")).await.unwrap();
    let mut second = report("two");
    second.context_tokens = 80;
    second.breakdown = json!({"tools":10,"messages":70});
    repo.record("alice", "alice", &second).await.unwrap();
    let d = totals(&repo, "alice", UsageQuery::default()).await;
    assert_eq!(d["totals"]["input_tokens"], 200);
    assert_eq!(d["contexts"][0]["context_tokens"], 80);
    assert_eq!(d["contexts"].as_array().unwrap().len(), 1);
}
#[tokio::test]
async fn invalid_counters_fail_instead_of_wrapping_and_deletion_cascades() {
    let (db, repo) = setup().await;
    let mut invalid = report("bad");
    invalid.input_tokens = u64::MAX;
    assert!(repo.record("alice", "alice", &invalid).await.is_err());
    repo.record("alice", "alice", &report("ok")).await.unwrap();
    sqlx::query("DELETE FROM conversations WHERE id='alice'")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(
        totals(&repo, "alice", UsageQuery::default()).await["totals"]["requests"],
        0
    );
}

#[tokio::test]
async fn chat_excludes_other_conversations_and_child_context_does_not_replace_parent() {
    let (db, repo) = setup().await;
    sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,model,created_at,updated_at) VALUES('child','alice','Child','aionrs',?,NULL,0,0)")
        .bind(r#"{"router_child":{"parent_conversation_id":"alice"}}"#).execute(db.pool()).await.unwrap();
    repo.record("alice", "alice", &report("root")).await.unwrap();
    let mut child = report("child");
    child.provider_id = Some("provider-b".into());
    child.model = "child-model".into();
    repo.record("alice", "child", &child).await.unwrap();
    let mut internal = report("internal");
    internal.is_child = true;
    internal.context_tokens = 999;
    repo.record("alice", "alice", &internal).await.unwrap();
    let q = UsageQuery {
        conversation_id: Some("alice".into()),
        ..Default::default()
    };
    let data = totals(&repo, "alice", q).await;
    assert_eq!(data["totals"]["requests"], 2);
    assert_eq!(
        data["contexts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["conversation_id"] == "alice")
            .unwrap()["context_tokens"],
        120
    );
    assert_eq!(
        totals(&repo, "alice", UsageQuery::default()).await["totals"]["requests"],
        3
    );
}

#[test]
fn hostile_period_boundaries_are_rejected_without_integer_overflow() {
    for query in [
        UsageQuery {
            end: Some(i64::MIN),
            ..Default::default()
        },
        UsageQuery {
            start: Some(0),
            end: Some(i64::MAX),
            ..Default::default()
        },
        UsageQuery {
            days: Some(0),
            ..Default::default()
        },
        UsageQuery {
            days: Some(366),
            ..Default::default()
        },
    ] {
        assert!(query.period().is_err());
    }
}

#[tokio::test]
async fn cumulative_reconnect_replay_and_out_of_order_reports_never_double_count() {
    let (_db, repo) = setup().await;
    let mut r = report("first");
    r.counter = Some(aionui_db::UsageCounter {
        source: "session-1".into(),
        baseline_if_unknown: false,
    });
    repo.record("alice", "alice", &r).await.unwrap();
    r.request_id = "duplicate-different-envelope".into();
    repo.record("alice", "alice", &r).await.unwrap();
    r.request_id = "second".into();
    r.input_tokens = 180;
    r.output_tokens = 35;
    r.cache_read_tokens = 60;
    repo.record("alice", "alice", &r).await.unwrap();
    r.request_id = "late".into();
    r.input_tokens = 100;
    r.output_tokens = 20;
    r.cache_read_tokens = 40;
    repo.record("alice", "alice", &r).await.unwrap();
    let data = totals(&repo, "alice", UsageQuery::default()).await;
    assert_eq!(data["totals"]["input_tokens"], 180);
    assert_eq!(data["totals"]["output_tokens"], 35);
    assert_eq!(data["totals"]["cache_read_tokens"], 60);
    assert_eq!(data["totals"]["requests"], 2);
}

#[tokio::test]
async fn unseen_resumed_session_establishes_baseline_without_charging_history() {
    let (_db, repo) = setup().await;
    let mut r = report("resume");
    r.counter = Some(aionui_db::UsageCounter {
        source: "resumed".into(),
        baseline_if_unknown: true,
    });
    repo.record("alice", "alice", &r).await.unwrap();
    assert_eq!(
        totals(&repo, "alice", UsageQuery::default()).await["totals"]["requests"],
        0
    );
    r.request_id = "after-resume".into();
    r.input_tokens += 50;
    r.output_tokens += 10;
    repo.record("alice", "alice", &r).await.unwrap();
    let data = totals(&repo, "alice", UsageQuery::default()).await;
    assert_eq!(data["totals"]["input_tokens"], 50);
    assert_eq!(data["totals"]["output_tokens"], 10);
}

#[tokio::test]
async fn context_only_adapters_have_context_without_fabricated_zero_billing() {
    let (_db, repo) = setup().await;
    let mut r = report("context-only");
    r.billing_reported = false;
    r.input_tokens = 0;
    r.output_tokens = 0;
    r.cache_read_tokens = 0;
    r.cache_creation_tokens = 0;
    r.context_tokens = 4096;
    repo.record("alice", "alice", &r).await.unwrap();
    let data = totals(&repo, "alice", UsageQuery::default()).await;
    assert_eq!(data["totals"]["requests"], 0);
    assert!(data["series"].as_array().unwrap().is_empty());
    assert_eq!(data["contexts"][0]["context_tokens"], 4096);
}

#[tokio::test]
async fn auxiliary_requests_count_spend_without_replacing_chat_context() {
    let (_db, repo) = setup().await;
    repo.record("alice", "alice", &report("chat-request")).await.unwrap();
    let mut auxiliary = report("compaction");
    auxiliary.context_reported = false;
    auxiliary.context_tokens = 0;
    repo.record("alice", "alice", &auxiliary).await.unwrap();
    let data = totals(&repo, "alice", UsageQuery::default()).await;
    assert_eq!(data["totals"]["requests"], 2);
    assert_eq!(data["totals"]["input_tokens"], 200);
    assert_eq!(data["contexts"][0]["context_tokens"], 120);
}

#[tokio::test]
async fn accounting_waits_for_competing_message_writer_without_losing_report() {
    let tmp = tempfile::tempdir().unwrap();
    let db = aionui_db::init_database(&tmp.path().join("usage.db")).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,model,created_at,updated_at) VALUES('chat','system_default_user','test','aionrs','{}','{}',0,0)")
        .execute(db.pool()).await.unwrap();
    let mut writer = db.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("UPDATE conversations SET name='message persisted' WHERE id='chat'")
        .execute(&mut *writer)
        .await
        .unwrap();
    let repo = SqliteUsageRepository::new(db.pool().clone());
    let recording = tokio::spawn(async move { repo.record("system_default_user", "chat", &report("racing")).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    writer.commit().await.unwrap();
    recording.await.unwrap().unwrap();
    let repo = SqliteUsageRepository::new(db.pool().clone());
    assert_eq!(
        totals(&repo, "system_default_user", UsageQuery::default()).await["totals"]["input_tokens"],
        100
    );
}

#[tokio::test]
async fn auxiliary_usage_resolves_the_owned_external_session_identity() {
    let (db, repo) = setup().await;
    sqlx::query("UPDATE conversations SET type='acp',model='{}' WHERE id='alice'")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO acp_session(conversation_id,agent_source,agent_id) VALUES('alice','custom','external-agent')",
    )
    .execute(db.pool())
    .await
    .unwrap();
    repo.record("alice", "alice", &report("memory-extraction"))
        .await
        .unwrap();
    let data = totals(&repo, "alice", UsageQuery::default()).await;
    assert_eq!(data["series"][0]["provider"], "external-agent");
}

#[tokio::test]
async fn chat_default_includes_history_older_than_a_year_but_global_stays_bounded() {
    let (db, repo) = setup().await;
    repo.record("alice", "alice", &report("old")).await.unwrap();
    sqlx::query("UPDATE usage_requests SET created_at=1000")
        .execute(db.pool())
        .await
        .unwrap();
    let q = UsageQuery {
        conversation_id: Some("alice".into()),
        ..Default::default()
    };
    let data = totals(&repo, "alice", q).await;
    assert_eq!(data["totals"]["requests"], 1);
    assert_eq!(data["start"], 0);
    assert_eq!(
        totals(&repo, "alice", UsageQuery::default()).await["totals"]["requests"],
        0
    );
}

#[tokio::test]
async fn request_measurements_survive_storage_without_becoming_billing_or_foreign_data() {
    let (_db, repo) = setup().await;
    let mut r = report("metrics");
    r.request_metrics = std::collections::BTreeMap::from([("system_chars".into(), 400), ("tool_count".into(), 12)]);
    repo.record("alice", "alice", &r).await.unwrap();
    let d = totals(&repo, "alice", UsageQuery::default()).await;
    assert_eq!(
        d["contexts"][0]["request_metrics"],
        json!({"system_chars":400,"tool_count":12})
    );
    assert_eq!(d["totals"]["input_tokens"], 100);
    assert!(
        totals(&repo, "bob", UsageQuery::default()).await["contexts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
