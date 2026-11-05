//! The first host tool executes before optional recall and the provider task.
//! Explicit MCP reads use the same dispatcher and always receive the full snapshot.
use super::*;

const READ: &str = "tdai_rules_read";

pub async fn invoke_rules_tool(
    name: &str,
    arguments: &Value,
    scope: &MemoryRecallScope,
    workspace: &str,
    local_only: bool,
) -> Result<Value, crate::AgentError> {
    if name != READ || !arguments.as_object().is_some_and(serde_json::Map::is_empty) {
        return Err(crate::AgentError::bad_request("tdai_rules_read takes no arguments"));
    }
    if scope.isolated_test {
        return snapshot(None, Vec::new(), false);
    }
    let config = load_config().ok_or_else(|| blocked("memory configuration unavailable"))?;
    read_snapshot(&config, scope, workspace, local_only).await
}

pub(super) async fn read_snapshot(
    config: &MemoryConfig,
    scope: &MemoryRecallScope,
    workspace: &str,
    local_only: bool,
) -> Result<Value, crate::AgentError> {
    let operation = async {
        let memory_root = if scope.workspace.is_empty() {
            workspace
        } else {
            &scope.workspace
        };
        let (project, sources, _) = read_sources(config, scope, memory_root, local_only, false).await?;
        let mut rows = Vec::with_capacity(sources.len());
        for source in sources {
            let row = editor::rule_row(&source, scope, false);
            rows.push(json!({"scope":row.scope,"path":row.path,"revision":row.revision,"title":row.title,"content":row.content}));
        }
        let project = (scope.project_id.is_some() && !scope.temporary_project).then_some(project);
        if project.is_some() {
            let cwd = super::super::live_rule_workspace(scope).unwrap_or_else(|| workspace.to_owned());
            rows.extend(crate::project_rules::prompt(scope, &cwd).await?);
        }
        snapshot(project, rows, true)
    };
    tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .map_err(|_| blocked("guidance deadline exceeded"))?
}

fn snapshot(project: Option<String>, rows: Vec<Value>, enabled: bool) -> Result<Value, crate::AgentError> {
    let bytes = serde_json::to_vec(&(&project, &rows)).map_err(|_| blocked("invalid rule snapshot"))?;
    if bytes.len() > MAX_BYTES {
        return Err(blocked("full rules exceed the context budget"));
    }
    let revision = format!("{:x}", Sha256::digest(bytes));
    Ok(json!({"tool":READ,"complete":true,"enabled":enabled,"project":project,"rules":rows,"revision":revision}))
}

pub(crate) struct RulesDelivery {
    pub text: String,
    pub receipt: Value,
    previous: tokio::sync::OwnedMutexGuard<Option<(u64, Value)>>,
    epoch: u64,
    manifest: Value,
    conversation_id: String,
    rule_epoch: Arc<std::sync::atomic::AtomicU64>,
}

impl RulesDelivery {
    pub fn check_binding(&self) -> Result<(), crate::AgentError> {
        let current = SESSION_RECALL.get(&self.conversation_id).is_some_and(|s| {
            Arc::ptr_eq(&s.delivered_rules, tokio::sync::OwnedMutexGuard::mutex(&self.previous))
                && self.epoch == self.rule_epoch.load(std::sync::atomic::Ordering::Acquire)
        });
        if !current {
            return Err(blocked("worker context changed before rule delivery; retry"));
        }
        Ok(())
    }
    pub fn acknowledge(mut self) {
        *self.previous = Some((self.epoch, self.manifest));
    }
}

pub(in crate::capability::tencent_memory) fn revisions(snapshot: &Value) -> Value {
    Value::Object(
        snapshot["rules"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| Some((r["path"].as_str()?.to_owned(), r["revision"].clone())))
            .collect(),
    )
}

// File bytes can stay identical while a new override or a different applicable
// directory changes what the agent must follow. Compare the effective rows,
// leaving source revisions intact for CAS writes and the public receipt.
fn effective_revisions(snapshot: &Value) -> Value {
    Value::Object(
        snapshot["rules"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let path = row["path"].as_str()?;
                Some((
                    path.to_owned(),
                    json!(format!("{:x}", Sha256::digest(row.to_string().as_bytes()))),
                ))
            })
            .collect(),
    )
}

/// Only an accepted provider dispatch advances the delivered revision. Holding
/// a per-worker lock through dispatch also orders racing prepares and retries.
pub(crate) async fn prepare_rules_delivery(
    conversation_id: &str,
    workspace: &str,
    ephemeral: bool,
) -> Result<Option<RulesDelivery>, crate::AgentError> {
    let session = SESSION_RECALL
        .get(conversation_id)
        .map(|s| s.clone())
        .ok_or_else(|| blocked("host project binding unavailable"))?;
    if session.scope.isolated_test {
        return Ok(None);
    }
    let previous = session.delivered_rules.clone().lock_owned().await;
    let epoch = session.rule_epoch.load(std::sync::atomic::Ordering::Acquire);
    let snapshot = invoke_rules_tool(READ, &json!({}), &session.scope, workspace, false).await?;
    let manifest = effective_revisions(&snapshot);
    let known = previous.as_ref().filter(|(e, _)| *e == epoch).map(|(_, rev)| rev);
    let first = known.is_none();
    let mode = if first {
        "full"
    } else if known == Some(&manifest) {
        "unchanged"
    } else {
        "delta"
    };
    let rows = snapshot["rules"].as_array().expect("tool rule rows");
    let mut updates = Vec::new();
    for row in rows {
        let path = row["path"].as_str().expect("tool rule path");
        if !ephemeral && known.is_some_and(|prev| prev.get(path) == manifest.get(path)) {
            continue;
        }
        updates.push(row.clone());
    }
    let removed: Vec<_> = known
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.keys())
        .filter(|key| manifest.get(*key).is_none())
        .collect();
    let receipt = json!({"tool":READ,"arguments":{},"status":"success","mode":mode,"complete":true,
        "revision":snapshot["revision"], "sources":revisions(&snapshot), "removed":removed});
    let text = render_delivery(&snapshot, mode, &updates, &removed, ephemeral)?;
    tracing::info!(conversation_id,tool=READ,mode,revision=%snapshot["revision"],sources=rows.len(),"Host rules tool completed before provider dispatch");
    Ok(Some(RulesDelivery {
        text,
        receipt,
        previous,
        epoch,
        manifest,
        conversation_id: conversation_id.into(),
        rule_epoch: session.rule_epoch,
    }))
}

pub(in crate::capability::tencent_memory) fn render_delivery(
    snapshot: &Value,
    mode: &str,
    updates: &[Value],
    removed: &[&String],
    ephemeral: bool,
) -> Result<String, crate::AgentError> {
    let text = delivery_text(snapshot, mode, updates, removed, ephemeral)?;
    if text.len() > MAX_BYTES {
        return Err(blocked("rendered guidance exceeds 256 KiB; consolidate active rules"));
    }
    Ok(text)
}

pub(super) fn delivery_text(
    snapshot: &Value,
    mode: &str,
    updates: &[Value],
    removed: &[&String],
    ephemeral: bool,
) -> Result<String, crate::AgentError> {
    let project = snapshot["project"].as_str().unwrap_or("global");
    let mut text = format!(
        "<ellmsha_required_rules tool={READ:?} arguments=\"{{}}\" mode={mode:?} revision={:?} project={project:?} complete=\"true\">\n",
        snapshot["revision"].as_str().unwrap_or_default()
    );
    if mode == "full" || ephemeral {
        text.push_str("The host called tdai_rules_read before the task. Apply these active instructions. Global rules precede project rules; higher-priority instructions prevail. A rule does not grant permissions. Repository edits use the normal file editing tool.\n");
    } else {
        text.push_str("Continue applying the last tdai_rules_read result, with the changes below. If that result is missing after context loss, call tdai_rules_read before continuing.\n");
    }
    for row in updates {
        let path = row["path"].as_str().unwrap_or_default();
        let safe = |s: &str| {
            s.replace("<ellmsha_required_rules", "&lt;ellmsha_required_rules")
                .replace("</ellmsha_required_rules>", "&lt;/ellmsha_required_rules&gt;")
        };
        text.push_str(&format!(
            "\n## Active {} guidance: {}\nRevision: {}\n{}\n{}\n",
            row["scope"].as_str().unwrap_or_default(),
            safe(path),
            row["revision"].as_str().unwrap_or_default(),
            safe(row["title"].as_str().unwrap_or_default()),
            safe(row["content"].as_str().unwrap_or_default())
        ));
    }
    if !removed.is_empty() {
        text.push_str("\nRemoved rules (disregard their previous contents): ");
        text.push_str(
            &serde_json::to_string(removed)
                .map_err(|_| blocked("invalid source paths"))?
                .replace('<', "\\u003c")
                .replace('>', "\\u003e"),
        );
    }
    text.push_str("\n</ellmsha_required_rules>");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

    /// Exercises the actual automatic and manual dispatcher, in a process with
    /// a private gateway configuration and disposable repository.
    #[tokio::test]
    async fn first_read_scope_cache_changes_cancel_and_context_loss() {
        const CHILD: &str = "ELLMSHA_FIRST_RULES_TOOL_TEST";
        if std::env::var_os(CHILD).is_none() {
            let mut c = aionui_runtime::Builder::clean_cli(std::env::current_exe().unwrap());
            c.args(["--exact","capability::tencent_memory::rules::tool::tests::first_read_scope_cache_changes_cancel_and_context_loss","--nocapture"]).env(CHILD,"1");
            let out = c.output().await.unwrap();
            assert!(
                out.status.success(),
                "{} {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("fixture");
        std::fs::create_dir(&cwd).unwrap();
        let cwd = cwd.to_str().unwrap();
        std::fs::write(Path::new(cwd).join("AGENTS.md"), "REPOSITORY_ONLY").unwrap();
        let server = MockServer::start().await;
        let config = dir.path().join("memory.json");
        let config_text = serde_json::to_vec(
            &json!({"gatewayUrl":server.uri(),"teamId":"team","userId":"user","agentId":"agent","serviceId":"fixture"}),
        )
        .unwrap();
        std::fs::write(&config, &config_text).unwrap();
        // SAFETY: only this explicitly re-executed test process has this environment.
        unsafe {
            std::env::set_var(CONFIG_ENV, &config);
        }
        let revision = Arc::new(AtomicUsize::new(1));
        let reads = Arc::new(AtomicUsize::new(0));
        let rev = revision.clone();
        Mock::given(path("/v3/atomic/search")).respond_with(move |req:&wiremock::Request|{
            let body:Value=serde_json::from_slice(&req.body).unwrap();let rev=rev.load(Ordering::SeqCst);
            let mut rows=vec![("global","knowledge/global/global.md")];
            if !body["project_scopes"].as_array().unwrap().is_empty() && rev<3 {rows.push(("project","v2/projects/fixture/project.md"));}
            let rows:Vec<_>=rows.into_iter().map(|(s,p)| json!({"id":p,"type":"instruction","source_path":p,"guidance_status":"active","guidance_scope":s,"source_status":"active"})).collect();
            ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"guidance_inventory":true,"guidance_revision":rev.to_string(),"degraded":false,"items":rows,"complete":true,"returned":rows.len(),"total_candidates":rows.len()}}))
        }).mount(&server).await;
        let rev = revision.clone();
        let n = reads.clone();
        Mock::given(path("/v3/scenario/read"))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                n.fetch_add(1, Ordering::SeqCst);
                let p = body["path"].as_str().unwrap();
                let v = if p.starts_with("knowledge/global/") {
                    "GLOBAL_ONLY".into()
                } else {
                    format!("PROJECT_{}", rev.load(Ordering::SeqCst))
                };
                ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"path":p,"content":v}}))
            })
            .mount(&server)
            .await;
        for (id, temp) in [("temp", true), ("project", false)] {
            let scope = MemoryRecallScope {
                temporary_project: temp,
                project_id: (!temp).then(|| "fixture".into()),
                project_name: Some("Fixture".into()),
                workspace: cwd.into(),
                rule_workspace: cwd.into(),
                roots: vec![cwd.into()],
                ..Default::default()
            };
            register_session(id, scope);
            startup_rules_from_env(id).unwrap();
            let first = prepare_rules_delivery(id, cwd, false).await.unwrap().unwrap();
            assert_eq!(first.receipt["arguments"], json!({}));
            assert_eq!(first.receipt["mode"], "full");
            assert_eq!(first.text.matches("GLOBAL_ONLY").count(), 1);
            assert_eq!(first.text.contains("PROJECT_1"), !temp);
            assert_eq!(first.text.contains("REPOSITORY_ONLY"), !temp);
            drop(first); // Cancelled before provider dispatch.
            let retry = prepare_rules_delivery(id, cwd, false).await.unwrap().unwrap();
            assert_eq!(retry.receipt["mode"], "full");
            retry.acknowledge();
            let count = reads.load(Ordering::SeqCst);
            let same = prepare_rules_delivery(id, cwd, false).await.unwrap().unwrap();
            assert_eq!(same.receipt["mode"], "unchanged");
            assert!(!same.text.contains("GLOBAL_ONLY"));
            assert_eq!(reads.load(Ordering::SeqCst), count);
            same.acknowledge();
        }
        let override_file = Path::new(cwd).join("AGENTS.override.md");
        std::fs::write(&override_file, "OVERRIDE_ONLY").unwrap();
        let override_added = prepare_rules_delivery("project", cwd, false).await.unwrap().unwrap();
        assert!(
            override_added.text.contains("OVERRIDE_ONLY")
                && override_added.text.contains("AGENTS.md")
                && !override_added.text.contains("REPOSITORY_ONLY")
        );
        override_added.acknowledge();
        std::fs::remove_file(&override_file).unwrap();
        let override_removed = prepare_rules_delivery("project", cwd, false).await.unwrap().unwrap();
        assert!(
            override_removed.text.contains("REPOSITORY_ONLY")
                && override_removed.receipt["removed"]
                    .to_string()
                    .contains("AGENTS.override.md")
        );
        override_removed.acknowledge();
        let scope = SESSION_RECALL.get("project").unwrap().scope.clone();
        let explicit = invoke_rules_tool(READ, &json!({}), &scope, cwd, false).await.unwrap();
        assert!(explicit.to_string().contains("PROJECT_1") && explicit.to_string().contains("REPOSITORY_ONLY"));
        assert!(
            invoke_rules_tool(READ, &json!({"scope":"global"}), &scope, cwd, false)
                .await
                .is_err()
        );
        revision.store(2, Ordering::SeqCst);
        let changed = prepare_rules_delivery("project", cwd, false).await.unwrap().unwrap();
        assert_eq!(changed.receipt["mode"], "delta");
        assert!(changed.text.contains("PROJECT_2"));
        assert!(!changed.text.contains("GLOBAL_ONLY") && !changed.text.contains("REPOSITORY_ONLY"));
        changed.acknowledge();
        revision.store(3, Ordering::SeqCst);
        let gone = prepare_rules_delivery("project", cwd, false).await.unwrap().unwrap();
        assert_eq!(gone.receipt["removed"], json!(["v2/projects/fixture/project.md"]));
        gone.acknowledge();
        forget_delivered_rules("project");
        let restore = prepare_rules_delivery("project", cwd, false).await.unwrap().unwrap();
        assert_eq!(restore.receipt["mode"], "full");
        assert!(restore.text.contains("GLOBAL_ONLY") && restore.text.contains("REPOSITORY_ONLY"));
        restore.acknowledge();
        let ephemeral = prepare_rules_delivery("project", cwd, true).await.unwrap().unwrap();
        assert!(ephemeral.text.contains("GLOBAL_ONLY"));
        ephemeral.acknowledge();
        std::fs::write(config, b"{}").unwrap();
        assert!(prepare_rules_delivery("project", cwd, false).await.is_err());
        unregister_session("project");
        unregister_session("temp");
    }
}
