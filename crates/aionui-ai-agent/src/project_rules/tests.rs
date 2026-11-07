use super::*;
use std::fs;
use tempfile::tempdir;

fn context(root: &Path) -> MemoryRecallScope {
    MemoryRecallScope {
        project_id: Some("project".into()),
        roots: vec![root.display().to_string()],
        workspace: root.display().to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn wide_metadata_tree_keeps_project_chat_rules_and_attached_repositories_available() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(root.join("AGENTS.md"), "Required root guidance").unwrap();
    let repo = root.join("client");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::write(repo.join("CLAUDE.md"), "Client guidance").unwrap();
    let metadata = root.join("metadata");
    fs::create_dir(&metadata).unwrap();
    for n in 0..4097 {
        fs::create_dir(metadata.join(n.to_string())).unwrap();
    }
    let scope = context(&root);
    assert_eq!(list(&scope).await.unwrap().files.len(), 2);
    let rows = prompt(&scope, repo.to_str().unwrap()).await.unwrap();
    assert!(rows.iter().any(|r| r["content"] == "Required root guidance"));
    assert!(rows.iter().any(|r| r["content"] == "Client guidance"));
}

#[tokio::test]
async fn nested_repositories_conditional_sources_and_native_precedence_share_one_inventory() {
    let root = tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let repo = root.join("apps/editor");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(root.join("vendor/secrets/.git")).unwrap();
    fs::write(root.join("AGENTS.md"), "Use the shared style.").unwrap();
    fs::write(repo.join("AGENTS.md"), "LOCAL BASE superseded").unwrap();
    fs::write(repo.join("AGENTS.override.md"), "Use only the EDITOR TOOLCHAIN.").unwrap();
    fs::write(repo.join("CLAUDE.md"), "# Auth\n\nNever echo credentials.").unwrap();
    fs::create_dir_all(repo.join(".claude/rules/backend")).unwrap();
    fs::write(
        repo.join(".claude/rules/backend/db.md"),
        "---\npaths:\n  - 'db/**'\n---\n\nConditional DB-specific rule.",
    )
    .unwrap();
    fs::create_dir_all(repo.join(".github/instructions")).unwrap();
    fs::write(
        repo.join(".github/instructions/review.instructions.md"),
        "---\napplyTo: 'src/**'\n---\nOnly src",
    )
    .unwrap();
    fs::create_dir_all(repo.join(".cursor/rules")).unwrap();
    fs::write(
        repo.join(".cursor/rules/python.mdc"),
        "---\nalwaysApply: false\n---\nManual Cursor body",
    )
    .unwrap();
    fs::write(repo.join("scope.md"), "Ordinary specifications aren't instructions.").unwrap();
    fs::write(root.join("vendor/secrets/AGENTS.md"), "Never read vendors").unwrap();
    let scope = context(&root);
    let files = list(&scope).await.unwrap();
    assert_eq!(files.project_id.as_deref(), Some("project"));
    assert_eq!(files.files.len(), 7);
    let selector = files.files.iter().find(|f| f.path.ends_with("db.md")).unwrap();
    assert!(
        selector
            .condition
            .as_deref()
            .is_some_and(|s| s.contains("./apps/editor") && s.contains("Files matching: db/**"))
    );
    let prompt_for = |rows: &Vec<serde_json::Value>| {
        rows.iter()
            .map(|row| row["content"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let umbrella = prompt_for(&prompt(&scope, root.to_str().unwrap()).await.unwrap());
    assert!(umbrella.contains("Use the shared style."));
    assert!(umbrella.contains("read this file before working there"));
    assert!(
        !umbrella.contains("EDITOR TOOLCHAIN")
            && !umbrella.contains("Conditional DB-specific")
            && !umbrella.contains("Manual Cursor body")
    );
    let targeted = prompt_for(&prompt(&scope, repo.to_str().unwrap()).await.unwrap());
    assert!(targeted.contains("EDITOR TOOLCHAIN") && targeted.contains("Never echo credentials"));
    assert!(!targeted.contains("LOCAL BASE superseded"));
    let mut temp = scope.clone();
    temp.temporary_project = true;
    assert!(list(&temp).await.unwrap().files.is_empty());
    assert!(prompt(&temp, root.to_str().unwrap()).await.unwrap().is_empty());
}

#[tokio::test]
async fn edits_are_revision_checked_and_preserve_frontmatter_managed_blocks_crlf_and_unrelated_sections() {
    let t = tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    let p = root.join("CLAUDE.md");
    let managed = "  <!-- BEGIN PROJECT-MEMORY MANAGED BLOCK --> \r\n## BMAD spec from historical memory\r\nOld managed data\r\n<!-- END PROJECT-MEMORY MANAGED BLOCK -->\r\n";
    let original = format!(
        "\u{feff}---\r\nunknown: keep\r\n---\r\n{managed}\r\n# Local title\r\n\r\nLine one.\r\n```md\r\n## sample heading\r\n```\r\n\r\n## Keep me\r\n\r\nOther bytes.\r\n"
    );
    fs::write(&p, &original).unwrap();
    let scope = context(&root);
    let f = list(&scope).await.unwrap().files.remove(0);
    assert_eq!(f.rules.len(), 2);
    assert!(f.rules[0].content.contains("## sample heading"));
    assert!(!f.rules.iter().any(|r| r.content.contains("BMAD")));
    let add = |revision: &str| ProjectFileRuleMutation::Add {
        file_id: f.id.clone(),
        revision: revision.into(),
        title: "Added".into(),
        content: "Extra instructions".into(),
    };
    let (a, b) = tokio::join!(mutate(&scope, add(&f.revision)), mutate(&scope, add(&f.revision)));
    assert!(matches!(
        (&a, &b),
        (Ok(_), Err(AgentError::Conflict(_))) | (Err(AgentError::Conflict(_)), Ok(_))
    ));
    assert!(fs::read_to_string(&p).unwrap().starts_with(&original));
    let f = list(&scope).await.unwrap().files.remove(0);
    mutate(
        &scope,
        ProjectFileRuleMutation::Edit {
            file_id: f.id.clone(),
            revision: f.revision.clone(),
            rule_id: f.rules[0].id.clone(),
            title: "Revised".into(),
            content: "Changed content\nSecond line".into(),
        },
    )
    .await
    .unwrap();
    let current = fs::read_to_string(&p).unwrap();
    assert!(current.contains(managed));
    assert!(current.starts_with("\u{feff}---\r\nunknown: keep\r\n---\r\n"));
    assert!(
        current.contains("# Revised\r\n\r\nChanged content\r\nSecond line\r\n\r\n## Keep me\r\n\r\nOther bytes.\r\n")
    );
    assert!(current.contains("## Added\r\n\r\nExtra instructions"));
    assert!(matches!(
        mutate(
            &scope,
            ProjectFileRuleMutation::Delete {
                file_id: f.id.clone(),
                revision: f.revision,
                rule_id: "0".into()
            }
        )
        .await,
        Err(AgentError::Conflict(_))
    ));
    let f = list(&scope).await.unwrap().files.remove(0);
    mutate(
        &scope,
        ProjectFileRuleMutation::Delete {
            file_id: f.id,
            revision: f.revision,
            rule_id: "2".into(),
        },
    )
    .await
    .unwrap();
    assert!(!fs::read_to_string(&p).unwrap().contains("Extra instructions"));
    let manifest = prompt(&scope, root.to_str().unwrap()).await.unwrap();
    assert!(
        manifest
            .iter()
            .any(|r| r["content"].as_str().unwrap().contains("Changed content"))
    );
    assert!(
        !manifest
            .iter()
            .any(|r| r["content"].as_str().unwrap().contains("Old managed data"))
    );
    fs::write(&p, managed).unwrap();
    let f = list(&scope).await.unwrap().files.remove(0);
    assert!(f.rules.is_empty());
    mutate(
        &scope,
        ProjectFileRuleMutation::Add {
            file_id: f.id,
            revision: f.revision,
            title: "First".into(),
            content: "Only manual rule".into(),
        },
    )
    .await
    .unwrap();
    assert!(fs::read_to_string(&p).unwrap().starts_with(managed));
}

#[cfg(unix)]
#[tokio::test]
async fn file_edits_cannot_cross_symlinks_or_accept_client_paths() {
    use std::os::unix::fs::symlink;
    let root = tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let exterior = tempdir().unwrap();
    let secret = exterior.path().join("AGENTS.md");
    fs::write(&secret, "secret").unwrap();
    symlink(&secret, path.join("AGENTS.md")).unwrap();
    fs::create_dir_all(exterior.path().join("rules")).unwrap();
    fs::write(exterior.path().join("rules/exposed.md"), "wrong").unwrap();
    symlink(exterior.path(), path.join(".claude")).unwrap();
    let scope = context(&path);
    assert!(list(&scope).await.unwrap().files.is_empty());
    assert!(
        mutate(
            &scope,
            ProjectFileRuleMutation::Add {
                file_id: secret.display().to_string(),
                revision: digest(b"secret"),
                title: "x".into(),
                content: "leak".into()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(fs::read_to_string(&secret).unwrap(), "secret");
    fs::write(path.join("GEMINI.md"), "simple").unwrap();
    let f = list(&scope).await.unwrap().files.remove(0);
    fs::remove_file(path.join("GEMINI.md")).unwrap();
    symlink(&secret, path.join("GEMINI.md")).unwrap();
    assert!(
        mutate(
            &scope,
            ProjectFileRuleMutation::Delete {
                file_id: f.id,
                revision: f.revision,
                rule_id: "0".into()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(fs::read_to_string(&secret).unwrap(), "secret");
}

#[tokio::test]
async fn live_native_worker_sends_changed_and_new_bodies_and_scopes_nested_modules() {
    let temp = tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repo = root.join("app");
    let module = repo.join("modules/checkout");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(&module).unwrap();
    let source = module.join("AGENTS.md");
    fs::write(&source, "Old module instruction").unwrap();
    fs::write(root.join("AGENTS.md"), "Umbrella guidance outside native git root").unwrap();
    let mut scope = context(&root);
    scope.rule_workspace = module.display().to_string();
    let files = list(&scope).await.unwrap().files;
    assert!(files.iter().any(|f| f.path == "./app/modules/checkout/AGENTS.md"));
    let manifest = || prompt(&scope, module.to_str().unwrap());
    let rows = manifest().await.unwrap();
    let render = |rows: &Vec<serde_json::Value>| {
        rows.iter()
            .map(|r| r["content"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        render(&rows).contains("Old module instruction"),
        "First resumed turn must hydrate current rules"
    );
    let same = render(&manifest().await.unwrap());
    assert!(same.contains("Old module instruction") && same.contains("Umbrella guidance outside native git root"));
    fs::write(&source, "New module instruction").unwrap();
    for _ in 0..2 {
        let now = render(&manifest().await.unwrap());
        assert!(now.contains("New module instruction") && !now.contains("Old module instruction"));
    }
    fs::write(&source, "Old module instruction").unwrap();
    assert!(
        render(&manifest().await.unwrap()).contains("Old module instruction"),
        "Once updated, reverting to the startup contents must still replace the previous turn's instructions"
    );
    fs::remove_file(&source).unwrap();
    let after = render(&manifest().await.unwrap());
    assert!(!after.contains("Old module instruction"));
    fs::write(repo.join("AGENTS.md"), "Instruction added after startup").unwrap();
    assert!(render(&manifest().await.unwrap()).contains("Instruction added after startup"));
}

#[tokio::test]
async fn heading_only_rules_empty_overrides_and_invalid_frontmatter_are_handled_explicitly() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::write(root.join("AGENTS.md"), "Fallback instructions").unwrap();
    fs::write(root.join("AGENTS.override.md"), "# Never commit BMAD artifacts\n").unwrap();
    let scope = context(&root);
    let rows = list(&scope).await.unwrap();
    let file = rows
        .files
        .iter()
        .find(|row| row.path == "./AGENTS.override.md")
        .unwrap();
    assert_eq!(file.rules[0].title, "Never commit BMAD artifacts");
    assert!(file.rules[0].content.is_empty());
    let payload = prompt(&scope, root.to_str().unwrap()).await.unwrap();
    assert!(
        payload
            .iter()
            .any(|p| p["content"].as_str().unwrap().contains("Never commit BMAD artifacts"))
    );
    assert!(
        !payload
            .iter()
            .any(|p| p["content"].as_str().unwrap().contains("Fallback instructions"))
    );
    fs::create_dir_all(root.join(".claude/rules")).unwrap();
    fs::write(
        root.join(".claude/rules/broken.md"),
        "---\npaths: ['secret/**']\nNever global",
    )
    .unwrap();
    assert!(list(&scope).await.is_err());
}

#[tokio::test]
async fn same_named_attached_roots_have_distinct_labels_and_distinct_write_destinations() {
    let tmp = tempdir().unwrap();
    let parent = tmp.path().canonicalize().unwrap();
    let one = parent.join("one/service");
    let two = parent.join("two/service");
    for root in [&one, &two] {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("AGENTS.md"), "Before").unwrap();
    }
    let mut scope = context(&one);
    scope.roots.push(two.display().to_string());
    let rows = list(&scope).await.unwrap().files;
    assert_eq!(rows[0].root_label, "one/service");
    assert_eq!(rows[1].root_label, "two/service");
    assert_ne!(rows[0].id, rows[1].id);
    mutate(
        &scope,
        ProjectFileRuleMutation::Add {
            file_id: rows[1].id.clone(),
            revision: rows[1].revision.clone(),
            title: "Second repository".into(),
            content: "Only here".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(fs::read_to_string(one.join("AGENTS.md")).unwrap(), "Before");
    assert!(fs::read_to_string(two.join("AGENTS.md")).unwrap().contains("Only here"));
}
