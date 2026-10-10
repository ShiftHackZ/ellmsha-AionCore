use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::SessionMcpTransport;
use aionui_extension::{ExtensionRegistry, ExtensionSource, ExtensionStateStore, ExtensionWatcher, ScanPath};
use aionui_realtime::BroadcastEventBus;

fn write_plugin(root: &Path, version: &str, marker: &str) {
    let plugin = root.join("fixture-plugin");
    std::fs::create_dir_all(plugin.join("skills/safe-echo")).unwrap();
    std::fs::write(
        plugin.join("skills/safe-echo/SKILL.md"),
        "---\nname: safe-echo\ndescription: safe fixture\n---\n",
    )
    .unwrap();
    std::fs::write(plugin.join("server.mjs"), format!("// {marker}\n")).unwrap();
    std::fs::write(
        plugin.join("aion-extension.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "name": "fixture-plugin",
            "version": version,
            "apiVersion": "1.0.0",
            "contributes": {
                "skills": [{
                    "name": "safe-echo",
                    "path": "skills/safe-echo/SKILL.md"
                }],
                "mcpServers": [{
                    "id": "safe-echo",
                    "name": "Safe Echo",
                    "transport": {
                        "type": "stdio",
                        "command": "node",
                        "args": ["-e", "$file:server.mjs"]
                    }
                }]
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn registry(root: &Path) -> ExtensionRegistry {
    let state_dir = root.join(".state");
    ExtensionRegistry::new(
        ExtensionStateStore::new(state_dir.join("states.json")),
        Arc::new(BroadcastEventBus::new(32)),
        "1.0.0".into(),
    )
}

fn scan(root: &Path) -> Vec<ScanPath> {
    vec![ScanPath {
        path: root.to_path_buf(),
        source: ExtensionSource::Local,
    }]
}

async fn wait_for_version(registry: &ExtensionRegistry, version: &str) {
    for _ in 0..40 {
        if registry
            .get_extension_by_name("fixture-plugin")
            .await
            .is_some_and(|extension| extension.manifest.version == version)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for plugin version {version}");
}

#[tokio::test]
async fn watcher_hotloads_valid_edit_retains_invalid_and_unloads_removed_plugin() {
    let temp = tempfile::tempdir().unwrap();
    write_plugin(temp.path(), "1.0.0", "first");
    let registry = registry(temp.path());
    registry.initialize_with_scan_paths(scan(temp.path())).await.unwrap();
    let first = registry.runtime_snapshot("user", Some("project")).await;
    let first_revision = Path::new(
        &registry
            .get_extension_by_name("fixture-plugin")
            .await
            .unwrap()
            .directory,
    )
    .to_path_buf();
    assert_eq!(first.skills[0].name, "safe-echo");
    assert_eq!(first.mcp_servers[0].name, "plugin:fixture-plugin:safe-echo");
    assert!(!first.mcp_servers[0].auto_approve_in_unattended);

    let _watcher = ExtensionWatcher::start(vec![temp.path().to_path_buf()], registry.clone()).unwrap();
    // Give the platform watcher a moment to finish registering before the edit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    write_plugin(temp.path(), "1.1.0", "edited");
    wait_for_version(&registry, "1.1.0").await;
    let edited = registry.runtime_snapshot("user", Some("project")).await;
    assert!(edited.generation > first.generation);
    let edited_extension = registry.get_extension_by_name("fixture-plugin").await.unwrap();
    let immutable_revision = Path::new(&edited_extension.directory).to_path_buf();
    assert_ne!(immutable_revision, temp.path().join("fixture-plugin"));
    assert!(immutable_revision.join("skills/safe-echo/SKILL.md").is_file());
    assert!(
        first_revision.exists(),
        "an in-flight snapshot must retain its revision"
    );
    match &edited.mcp_servers[0].transport {
        SessionMcpTransport::Stdio { args, .. } => {
            assert!(args.get(1).is_some_and(|script| script.contains("edited")));
        }
        other => panic!("unexpected fixture MCP transport: {other:?}"),
    }
    drop(first);
    assert!(
        !first_revision.exists(),
        "an unreferenced superseded revision must be pruned"
    );

    std::fs::write(
        temp.path().join("fixture-plugin/skills/safe-echo/SKILL.md"),
        b"---\nname: [broken\ndescription:\n---\n",
    )
    .unwrap();
    registry.hot_reload().await;
    let retained = registry.get_extension_by_name("fixture-plugin").await.unwrap();
    assert_eq!(Path::new(&retained.directory), immutable_revision);

    std::fs::write(
        temp.path().join("fixture-plugin/skills/safe-echo/SKILL.md"),
        b"---\nname: safe-echo\ndescription: safe fixture\n---\n",
    )
    .unwrap();

    // A syntactically broken direct edit must not replace the complete working
    // revision, including its resolved MCP script and skill source.
    std::fs::write(temp.path().join("fixture-plugin/server.mjs"), b"const =\n").unwrap();
    registry.hot_reload().await;
    let retained = registry.get_extension_by_name("fixture-plugin").await.unwrap();
    assert_eq!(retained.manifest.version, "1.1.0");
    assert_eq!(Path::new(&retained.directory), immutable_revision);
    let retained_snapshot = registry.runtime_snapshot("user", Some("project")).await;
    match &retained_snapshot.mcp_servers[0].transport {
        SessionMcpTransport::Stdio { args, .. } => {
            assert!(args.get(1).is_some_and(|script| script.contains("edited")));
        }
        other => panic!("unexpected fixture MCP transport: {other:?}"),
    }

    // Removing the source directory is an intentional uninstall. A malformed
    // update above is retained because its source directory still exists.
    std::fs::remove_dir_all(temp.path().join("fixture-plugin")).unwrap();
    for _ in 0..40 {
        if registry.get_extension_by_name("fixture-plugin").await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(registry.get_extension_by_name("fixture-plugin").await.is_none());
    assert!(
        registry
            .runtime_snapshot("user", Some("project"))
            .await
            .skills
            .is_empty()
    );
}

#[tokio::test]
async fn global_and_project_bindings_filter_skills_and_mcp_for_parent_and_subagent() {
    let temp = tempfile::tempdir().unwrap();
    write_plugin(temp.path(), "1.0.0", "first");
    let registry = registry(temp.path());
    registry.initialize_with_scan_paths(scan(temp.path())).await.unwrap();

    registry
        .disable_extension_for_user("user", "fixture-plugin", None)
        .await
        .unwrap();
    assert!(registry.runtime_snapshot("user", None).await.skills.is_empty());

    registry
        .set_extension_enabled_for_project("user", "project-a", "fixture-plugin", true)
        .await
        .unwrap();
    let parent = registry.runtime_snapshot("user", Some("project-a")).await;
    let subagent = registry.runtime_snapshot("user", Some("project-a")).await;
    assert_eq!(parent.skills, subagent.skills);
    assert_eq!(parent.mcp_servers.len(), 1);
    assert!(!parent.mcp_servers[0].auto_approve_in_unattended);
    assert!(!subagent.mcp_servers[0].auto_approve_in_unattended);
    assert!(
        registry
            .runtime_snapshot("user", Some("project-b"))
            .await
            .skills
            .is_empty()
    );

    registry
        .set_extension_enabled_for_project("user", "project-a", "fixture-plugin", false)
        .await
        .unwrap();
    assert!(
        registry
            .runtime_snapshot("user", Some("project-a"))
            .await
            .mcp_servers
            .is_empty()
    );
}

#[tokio::test]
async fn project_bindings_persist_across_store_reload() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("extension-states.json");
    let store = ExtensionStateStore::new(file.clone());
    store
        .set_project_enabled("user", "project-a", "fixture-plugin", false)
        .await
        .unwrap();
    let reloaded = ExtensionStateStore::new(file);
    reloaded.load().await.unwrap();
    assert_eq!(
        reloaded
            .get_project_enabled("user", "project-a", "fixture-plugin")
            .await,
        Some(false)
    );
}

#[tokio::test]
async fn test_fixture_discovers_safe_skill_and_mcp_runtime() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sd05-fixture");
    assert!(fixture.join("skills/sd05-safe-echo/SKILL.md").is_file());

    let temp = tempfile::tempdir().unwrap();
    let registry = registry(temp.path());
    registry
        .initialize_with_scan_paths(vec![ScanPath {
            path: fixture.parent().unwrap().to_path_buf(),
            source: ExtensionSource::Local,
        }])
        .await
        .unwrap();
    let snapshot = registry.runtime_snapshot("user", Some("project")).await;
    assert!(snapshot.skills.iter().any(|skill| skill.name == "sd05-safe-echo"));
    let server = snapshot
        .mcp_servers
        .iter()
        .find(|server| server.id == "plugin:sd05-fixture:safe-echo")
        .expect("fixture MCP server must be discovered");
    assert!(!server.auto_approve_in_unattended);
    match &server.transport {
        SessionMcpTransport::Stdio { command, args, .. } => {
            assert_eq!(command, "node");
            assert_eq!(args.first().map(String::as_str), Some("-e"));
            assert!(args.get(1).is_some_and(|script| script.contains("safe_echo")));
        }
        other => panic!("unexpected fixture MCP transport: {other:?}"),
    }
}

#[tokio::test]
async fn repository_builtin_native_plugins_are_pinned_and_discoverable() {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("extension crate must be inside the v3 repository");
    let plugins = repository_root.join("plugins");
    assert!(plugins.join("mobile/LICENSE").is_file());
    assert!(plugins.join("mobile/skills/mobile-automation/SKILL.md").is_file());
    assert!(plugins.join("computer-use/LICENSE").is_file());
    assert!(plugins.join("computer-use/skills/open-computer-use/SKILL.md").is_file());
    assert!(plugins.join("plugin-dev/skills/plugin-authoring/SKILL.md").is_file());
    assert!(plugins.join("plugin-dev/templates/basic-plugin/mcp.json").is_file());
    assert!(
        !plugins.join("sd05-fixture/aion-extension.json").exists(),
        "test fixtures must not be production plugins"
    );

    let temp = tempfile::tempdir().unwrap();
    let registry = registry(temp.path());
    registry
        .initialize_with_scan_paths(vec![ScanPath {
            path: plugins,
            source: ExtensionSource::Builtin,
        }])
        .await
        .unwrap();
    assert_eq!(
        registry
            .get_extension_by_name("mobile")
            .await
            .expect("mobile plugin must be loaded")
            .manifest
            .version,
        "1.0.8"
    );
    assert_eq!(
        registry
            .get_extension_by_name("computer-use")
            .await
            .expect("Open Computer Use plugin must be loaded")
            .manifest
            .version,
        "0.3.6"
    );
    let snapshot = registry.runtime_snapshot("user", Some("project")).await;
    assert!(snapshot.skills.iter().any(|skill| skill.name == "mobile-automation"));
    assert!(snapshot.skills.iter().any(|skill| skill.name == "open-computer-use"));
    assert!(snapshot.skills.iter().any(|skill| skill.name == "plugin-authoring"));
    assert!(!snapshot.skills.iter().any(|skill| skill.name == "sd05-safe-echo"));
    let server = snapshot
        .mcp_servers
        .iter()
        .find(|server| server.id == "plugin:mobile:mobile-mcp")
        .expect("Mobile Next MCP server must be discovered");
    assert!(server.auto_approve_in_unattended);
    match &server.transport {
        SessionMcpTransport::Stdio { command, args, env } => {
            assert_eq!(command, "node");
            assert_eq!(args.first().map(String::as_str), Some("-e"));
            assert!(
                args.get(1)
                    .is_some_and(|script| script.contains("ELLM_SHA_MOBILE_MCP_ENTRY"))
            );
            assert!(!args.iter().any(|arg| arg.contains("@mobilenext/mobile-mcp")));
            assert_eq!(env.get("MOBILEMCP_DISABLE_TELEMETRY").map(String::as_str), Some("1"));
        }
        other => panic!("unexpected Mobile Next MCP transport: {other:?}"),
    }
    let computer_use = snapshot
        .mcp_servers
        .iter()
        .find(|server| server.id == "plugin:computer-use:open-computer-use")
        .expect("Open Computer Use MCP server must be discovered");
    assert!(!computer_use.auto_approve_in_unattended);
    match &computer_use.transport {
        SessionMcpTransport::Stdio { command, args, env } => {
            assert_eq!(command, "node");
            assert_eq!(args.first().map(String::as_str), Some("-e"));
            assert!(args.get(1).is_some_and(|script| {
                script.contains("ELLM_SHA_OPEN_COMPUTER_USE_ENTRY") && script.contains("['mcp']")
            }));
            assert!(!args.iter().any(|arg| arg.contains("npx") || arg.contains("npm")));
            assert_eq!(
                env.get("ELLM_SHA_OPEN_COMPUTER_USE_ENTRY").map(String::as_str),
                Some("")
            );
        }
        other => panic!("unexpected Open Computer Use MCP transport: {other:?}"),
    }
}
