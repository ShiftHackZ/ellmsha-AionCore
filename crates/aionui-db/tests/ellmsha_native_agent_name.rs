use sqlx::sqlite::SqlitePoolOptions;

#[tokio::test]
async fn native_name_migration_preserves_custom_names_and_runtime_identity() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE agent_metadata (
            id TEXT PRIMARY KEY, name TEXT, agent_type TEXT, agent_source TEXT,
            command TEXT, updated_at INTEGER
        );
        INSERT INTO agent_metadata VALUES
          ('bundled', 'Aion CLI', 'aionrs', 'internal', NULL, 1),
          ('legacy', 'AionCLI', 'aionrs', 'internal', NULL, 1),
          ('renamed', 'My assistant', 'aionrs', 'internal', NULL, 1),
          ('custom', 'Aion CLI', 'aionrs', 'custom', 'custom-native', 1),
          ('other', 'Aion CLI', 'acp', 'builtin', 'aioncli', 1);",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/037_ellmsha_native_agent_name.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let rows: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT id, name, agent_type, command FROM agent_metadata ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("bundled".into(), "eLLMsha".into(), "aionrs".into(), None),
            (
                "custom".into(),
                "Aion CLI".into(),
                "aionrs".into(),
                Some("custom-native".into())
            ),
            ("legacy".into(), "eLLMsha".into(), "aionrs".into(), None),
            ("other".into(), "Aion CLI".into(), "acp".into(), Some("aioncli".into())),
            ("renamed".into(), "My assistant".into(), "aionrs".into(), None),
        ]
    );
}
