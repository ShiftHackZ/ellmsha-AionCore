use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use aionui_extension::{
    ExtensionRegistry, ExtensionSource, ExtensionStateStore, ExtensionWatcher, PluginInstallSource, PluginInstaller,
    ScanPath,
};
use aionui_realtime::BroadcastEventBus;

fn registry(root: &Path) -> ExtensionRegistry {
    ExtensionRegistry::new(
        ExtensionStateStore::new(root.join("state.json")),
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

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

#[tokio::test]
async fn imports_real_codex_and_claude_layouts_with_truthful_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    copy_tree(&fixture("sd07-codex"), &temp.path().join("sd07-codex"));
    copy_tree(&fixture("sd07-claude"), &temp.path().join("sd07-claude"));
    let registry = registry(temp.path());
    registry.initialize_with_scan_paths(scan(temp.path())).await.unwrap();

    let summaries = registry.get_loaded_extensions_for_user("user-a").await;
    let codex = summaries.iter().find(|item| item.name == "sd07-codex-fixture").unwrap();
    let codex_compat = codex.compatibility.as_ref().unwrap();
    assert_eq!(codex_compat.supported_components, ["skills", "mcp_servers"]);
    assert!(
        codex_compat
            .unsupported_components
            .iter()
            .any(|notice| notice.component == "hooks")
    );
    let claude = summaries
        .iter()
        .find(|item| item.name == "sd07-claude-fixture")
        .unwrap();
    let claude_compat = claude.compatibility.as_ref().unwrap();
    assert!(
        claude_compat
            .unsupported_components
            .iter()
            .any(|notice| notice.component == "agents")
    );
    assert!(
        claude_compat
            .unsupported_components
            .iter()
            .any(|notice| notice.component == "lspServers")
    );
    assert!(
        claude_compat
            .unsupported_components
            .iter()
            .any(|notice| notice.component == "hooks")
    );
    assert!(
        claude_compat
            .unsupported_components
            .iter()
            .any(|notice| notice.component == "outputStyles")
    );

    let snapshot = registry.runtime_snapshot("user-a", None).await;
    assert!(snapshot.skills.iter().any(|skill| skill.name == "codex-fixture"));
    assert!(snapshot.skills.iter().any(|skill| skill.name == "claude-fixture"));
    assert!(snapshot.skills.iter().any(|skill| skill.name == "claude-root-fixture"));
    let imported_servers = snapshot
        .mcp_servers
        .iter()
        .filter(|server| server.id.contains("sd07-"))
        .collect::<Vec<_>>();
    assert_eq!(imported_servers.len(), 2);
    assert!(
        imported_servers.iter().all(|server| !server.auto_approve_in_unattended),
        "third-party package formats must remain approval gated"
    );
}

#[tokio::test]
async fn imported_plugin_reload_retains_last_good_and_propagates_disable_and_removal() {
    let temp = tempfile::tempdir().unwrap();
    let plugin = temp.path().join("sd07-codex");
    copy_tree(&fixture("sd07-codex"), &plugin);
    let registry = registry(temp.path());
    registry.initialize_with_scan_paths(scan(temp.path())).await.unwrap();
    let first = registry.get_extension_by_name("sd07-codex-fixture").await.unwrap();
    assert_eq!(first.manifest.version, "1.2.3");

    let _watcher = ExtensionWatcher::start(vec![temp.path().to_path_buf()], registry.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    std::fs::write(plugin.join(".codex-plugin/plugin.json"), b"{ broken").unwrap();
    registry.hot_reload().await;
    let retained = registry.get_extension_by_name("sd07-codex-fixture").await.unwrap();
    assert_eq!(retained.manifest.version, "1.2.3");

    registry
        .disable_extension_for_user("user-a", "sd07-codex-fixture", Some("test"))
        .await
        .unwrap();
    let disabled = registry.runtime_snapshot("user-a", None).await;
    assert!(!disabled.skills.iter().any(|skill| skill.name == "codex-fixture"));
    assert!(
        !disabled
            .mcp_servers
            .iter()
            .any(|server| server.id.contains("sd07-codex"))
    );

    std::fs::remove_dir_all(&plugin).unwrap();
    registry.hot_reload().await;
    assert!(registry.get_extension_by_name("sd07-codex-fixture").await.is_none());
}

#[tokio::test]
async fn installer_imports_local_and_git_packages_and_uninstall_propagates() {
    let temp = tempfile::tempdir().unwrap();
    let install_root = temp.path().join("installed");
    std::fs::create_dir_all(&install_root).unwrap();
    let registry = registry(temp.path());
    registry.initialize_with_scan_paths(scan(&install_root)).await.unwrap();
    let installer = PluginInstaller::new(install_root.clone(), registry.clone());

    let hook_marker = temp.path().join("untrusted-hook-ran");
    let native = temp.path().join("native-with-hook");
    std::fs::create_dir_all(&native).unwrap();
    let hook = native.join("activate.sh");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", hook_marker.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        native.join("aion-extension.json"),
        br#"{"name":"native-with-hook","version":"1.0.0","lifecycle":{"onActivate":"activate.sh"}}"#,
    )
    .unwrap();
    installer
        .install(PluginInstallSource::LocalPath { path: native })
        .await
        .unwrap();
    assert!(
        !hook_marker.exists(),
        "local plugin hooks must not execute without trust"
    );

    let local = installer
        .install(PluginInstallSource::LocalPath {
            path: fixture("sd07-codex"),
        })
        .await
        .unwrap();
    assert_eq!(local.name, "sd07-codex-fixture");
    assert!(
        registry
            .runtime_snapshot("user-a", None)
            .await
            .skills
            .iter()
            .any(|skill| skill.name == "codex-fixture")
    );
    installer.uninstall("sd07-codex-fixture").await.unwrap();
    assert!(registry.get_extension_by_name("sd07-codex-fixture").await.is_none());

    let authoring_template = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .join("plugins/plugin-dev/templates/basic-plugin");
    let authored = installer
        .install(PluginInstallSource::LocalPath {
            path: authoring_template,
        })
        .await
        .unwrap();
    assert_eq!(authored.name, "example-safe-plugin");
    let authored_snapshot = registry.runtime_snapshot("user-a", None).await;
    assert!(authored_snapshot.skills.iter().any(|skill| skill.name == "safe-echo"));
    assert!(
        authored_snapshot
            .mcp_servers
            .iter()
            .find(|server| server.id == "plugin:example-safe-plugin:safe-echo")
            .is_some_and(|server| !server.auto_approve_in_unattended)
    );
    installer.uninstall("example-safe-plugin").await.unwrap();

    let git_source = temp.path().join("git-source");
    copy_tree(&fixture("sd07-claude"), &git_source);
    for args in [
        vec!["init"],
        vec!["config", "user.email", "fixture@example.invalid"],
        vec!["config", "user.name", "Fixture"],
        vec!["add", "."],
        vec!["commit", "-m", "fixture"],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&git_source)
                .status()
                .unwrap()
                .success()
        );
    }
    let git = installer
        .install(PluginInstallSource::Git {
            url: git_source.to_string_lossy().into_owned(),
            git_ref: None,
            subdirectory: None,
        })
        .await
        .unwrap();
    assert_eq!(git.name, "sd07-claude-fixture");
}

#[tokio::test]
async fn installer_rejects_malformed_escape_and_unsupported_only_packages_without_replacing_working_version() {
    let temp = tempfile::tempdir().unwrap();
    let install_root = temp.path().join("installed");
    std::fs::create_dir_all(&install_root).unwrap();
    let registry = registry(temp.path());
    registry.initialize_with_scan_paths(scan(&install_root)).await.unwrap();
    let installer = PluginInstaller::new(install_root, registry.clone());

    installer
        .install(PluginInstallSource::LocalPath {
            path: fixture("sd07-codex"),
        })
        .await
        .unwrap();

    let malformed = temp.path().join("malformed");
    copy_tree(&fixture("sd07-codex"), &malformed);
    std::fs::write(malformed.join(".codex-plugin/plugin.json"), b"not json").unwrap();
    let error = installer
        .install(PluginInstallSource::LocalPath { path: malformed })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid JSON"));
    assert_eq!(
        registry
            .get_extension_by_name("sd07-codex-fixture")
            .await
            .unwrap()
            .manifest
            .version,
        "1.2.3"
    );

    let escape = temp.path().join("escape");
    std::fs::create_dir_all(escape.join(".codex-plugin")).unwrap();
    std::fs::write(
        escape.join(".codex-plugin/plugin.json"),
        br#"{"name":"escape-plugin","version":"1.0.0","skills":"../outside"}"#,
    )
    .unwrap();
    let error = installer
        .install(PluginInstallSource::LocalPath { path: escape })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Path traversal"));

    let hook_escape = temp.path().join("hook-escape");
    std::fs::create_dir_all(&hook_escape).unwrap();
    std::fs::write(temp.path().join("outside.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(
        hook_escape.join("aion-extension.json"),
        br#"{"name":"hook-escape","version":"1.0.0","lifecycle":{"onActivate":"../outside.sh"}}"#,
    )
    .unwrap();
    let error = installer
        .install(PluginInstallSource::LocalPath { path: hook_escape })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Path traversal"));

    let unsupported = temp.path().join("unsupported");
    std::fs::create_dir_all(unsupported.join(".claude-plugin")).unwrap();
    std::fs::write(
        unsupported.join(".claude-plugin/plugin.json"),
        br#"{"name":"unsupported-plugin","version":"1.0.0","hooks":"./hooks.json"}"#,
    )
    .unwrap();
    let error = installer
        .install(PluginInstallSource::LocalPath { path: unsupported })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no supported skills or MCP servers"));
    assert!(error.to_string().contains("hooks"));

    let authenticated = temp.path().join("authenticated-mcp");
    std::fs::create_dir_all(authenticated.join(".codex-plugin")).unwrap();
    std::fs::write(
        authenticated.join(".codex-plugin/plugin.json"),
        br#"{"name":"authenticated-mcp","version":"1.0.0","mcpServers":{"private":{"type":"streamable-http","url":"https://example.invalid/mcp","extensions":{"com.openai":{"auth":{"type":"oauth"}}}}}}"#,
    )
    .unwrap();
    let error = installer
        .install(PluginInstallSource::LocalPath { path: authenticated })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("mcp_auth"));
}
