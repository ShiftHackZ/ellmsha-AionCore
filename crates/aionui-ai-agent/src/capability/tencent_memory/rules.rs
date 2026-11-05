//! Mandatory, complete guidance barrier. Optional topical recall cannot satisfy it.
use std::collections::BTreeMap;

use super::*;

mod editor;
mod tool;
pub use editor::{mutate_chat_rule, read_chat_rules};
pub use tool::invoke_rules_tool;
pub(crate) use tool::{RulesDelivery, prepare_rules_delivery};
pub(super) use tool::{render_delivery, revisions};

const PAGE: usize = super::RULE_PAGE_SIZE;
const MAX_RULES: usize = 512;
const MAX_BYTES: usize = 256 * 1024;

fn blocked(reason: &str) -> crate::AgentError {
    tracing::warn!(reason, "Mandatory project rules preflight blocked provider dispatch");
    crate::AgentError::RequiredGuidanceUnavailable(format!(
        "Project rules could not be loaded completely ({reason}). The task was not sent to the model. Restore TencentDB memory or resolve the guidance limit, then retry."
    ))
}

async fn inventory(
    client: &reqwest::Client,
    config: &MemoryConfig,
    scopes: &[String],
    aliases: &[String],
    local_only: bool,
    max_rules: usize,
) -> Result<(BTreeMap<String, Value>, Option<(String, bool)>), crate::AgentError> {
    let mut records = BTreeMap::new();
    let mut offset = 0;
    let mut expected_total = None;
    let mut revision = None;
    loop {
        let mut body = rule_body(&plan("project guidance", None), scopes, aliases);
        body["guidance_inventory"] = json!(true);
        body["limit"] = json!(PAGE);
        body["offset"] = json!(offset);
        let page = call(client, config, "atomic/search", body).await.map_err(blocked)?;
        if page.get("guidance_inventory").and_then(Value::as_bool) != Some(true) {
            return Err(blocked(
                "memory service lacks mandatory guidance support; restart or update the service",
            ));
        }
        if page.get("degraded").and_then(Value::as_bool) != Some(false) {
            return Err(blocked("guidance inventory unavailable or incomplete"));
        }
        if local_only && page.get("guidance_locality").and_then(Value::as_str) != Some("local") {
            return Err(blocked("memory locality is unverified for local-only routing"));
        }
        let page_revision = page
            .get("guidance_revision")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(|revision| {
                (
                    revision.to_owned(),
                    page.get("guidance_write_version").and_then(Value::as_u64) == Some(1),
                )
            });
        if offset != 0 && revision != page_revision {
            return Err(blocked("guidance changed while reading pages"));
        }
        revision = page_revision;
        let total = page
            .get("total_candidates")
            .and_then(Value::as_u64)
            .ok_or_else(|| blocked("missing guidance count"))?;
        if total > max_rules as u64 || expected_total.is_some_and(|previous| previous != total) {
            return Err(blocked("guidance inventory changed or exceeds the record limit"));
        }
        expected_total = Some(total);
        let items = page
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| blocked("invalid guidance page"))?;
        if items.len() > PAGE || page.get("returned").and_then(Value::as_u64) != Some(items.len() as u64) {
            return Err(blocked("invalid guidance page size"));
        }
        for item in items {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| blocked("guidance identity missing"))?;
            if records.insert(id.to_owned(), item.clone()).is_some() {
                return Err(blocked("duplicate guidance cursor"));
            }
        }
        if page.get("complete").and_then(Value::as_bool) == Some(true) {
            if records.len() as u64 != total || !page.get("next_offset").is_none_or(Value::is_null) {
                return Err(blocked("guidance count mismatch"));
            }
            return Ok((records, revision));
        }
        let next = page
            .get("next_offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| blocked("guidance cursor missing"))?;
        if items.is_empty() || next != offset + items.len() as u64 || next >= total {
            return Err(blocked("guidance cursor did not advance"));
        }
        offset = next;
    }
}

fn applicable_source(path: &str, scope: &str, scopes: &[String], aliases: &[String]) -> bool {
    if path.contains(['\0', '\\'])
        || !path.ends_with(".md")
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return false;
    }
    let path = path.to_ascii_lowercase();
    let global = ["knowledge/global/", "global/", "v2/wiki/", "wiki/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
        || path.contains("/global-rules/");
    if global || scope == "global" {
        return global && scope == "global";
    }
    if scopes.iter().any(|id| path.contains(&format!("/{id}/"))) {
        return true;
    }
    let Some(id) = path
        .strip_prefix("v2/projects/")
        .and_then(|rest| rest.split_once('/'))
        .map(|(id, _)| id)
    else {
        return false;
    };
    static VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-v?\d+(?:-\d+)*$").expect("static alias regex"));
    aliases.iter().any(|alias| {
        let normalized = slug(alias);
        [normalized.as_str(), VERSION.replace(&normalized, "").as_ref()]
            .into_iter()
            .any(|name| {
                name.len() >= 4
                    && (id == name
                        || id
                            .strip_prefix(name)
                            .and_then(|rest| rest.strip_prefix(['-', '_']))
                            .is_some_and(|tail| {
                                tail.split(['-', '_'])
                                    .all(|part| part.len() >= 2 && part.bytes().all(|c| c.is_ascii_hexdigit()))
                            }))
            })
    })
}

#[derive(Clone)]
pub(super) struct VerifiedSource {
    rule: Value,
    record: Value,
}

pub(super) type SourceCache = Arc<Mutex<Option<(String, Vec<VerifiedSource>)>>>;

async fn read_sources(
    config: &MemoryConfig,
    scope: &MemoryRecallScope,
    workspace: &str,
    local_only: bool,
    editing: bool,
) -> Result<(String, Vec<VerifiedSource>, Option<String>), crate::AgentError> {
    if local_only
        && !reqwest::Url::parse(&config.gateway_url)
            .ok()
            .is_some_and(|url| url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1")))
    {
        return Err(blocked("local-only memory gateway is not loopback"));
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| blocked("client unavailable"))?;
    let (project, scopes, aliases) = if scope.project_id.is_some() && !scope.temporary_project {
        derive_aliases(scope, workspace)
    } else {
        // A temporary working folder can itself be nested in another project's Git tree.
        (project_scope(workspace), vec![], vec![])
    };
    // The panel can repair an oversized set. Provider dispatch retains its original hard limits.
    let max_rules = if editing { MAX_RULES * 2 } else { MAX_RULES };
    let max_bytes = if editing { MAX_BYTES * 2 } else { MAX_BYTES };
    let before = inventory(&client, config, &scopes, &aliases, local_only, max_rules).await?;
    let session_cache = scope.conversation_id.as_ref().and_then(|id| {
        SESSION_RECALL
            .get(id)
            .filter(|entry| entry.scope == *scope)
            .map(|s| s.rule_sources.clone())
    });
    let mut paths = std::collections::BTreeSet::new();
    if before
        .0
        .values()
        .filter_map(|record| record["source_path"].as_str())
        .any(|path| !paths.insert(path))
    {
        return Err(blocked("multiple active records refer to the same guidance source"));
    }
    // A revision is the gateway's scenario epoch. Never trust an old snapshot
    // without a fresh authenticated inventory and exactly the same indexed rows.
    let cache_key = if !editing && before.1.is_some() {
        let identity = serde_json::to_vec(&(
            &config.gateway_url,
            &config.service_id,
            &config.team_id,
            &config.user_id,
            &config.agent_id,
            &config.gateway_api_key,
            local_only,
            &project,
            &before,
        ))
        .map_err(|_| blocked("invalid guidance inventory"))?;
        Some(format!("{:x}", Sha256::digest(identity)))
    } else {
        None
    };
    if let (Some(cache), Some(key)) = (&session_cache, &cache_key) {
        let cache = cache.lock().map_err(|_| blocked("guidance cache unavailable"))?;
        if let Some((_, sources)) = cache.as_ref().filter(|(old, _)| old == key) {
            return Ok((
                project,
                sources.clone(),
                before.1.and_then(|(rev, writable)| writable.then_some(rev)),
            ));
        }
    }
    let mut rules = Vec::new();
    let mut bytes = 0;
    for item in before.0.values() {
        if item.get("guidance_status").and_then(Value::as_str) != Some("active")
            || item.get("source_status").and_then(Value::as_str) != Some("active")
            || !matches!(
                item.get("type").and_then(Value::as_str),
                Some("instruction" | "persona")
            )
        {
            return Err(blocked("active guidance inventory contains an unverified record"));
        }
        let guidance_scope = item
            .get("guidance_scope")
            .and_then(Value::as_str)
            .filter(|scope| matches!(*scope, "global" | "project"))
            .ok_or_else(|| blocked("guidance scope missing"))?;
        let path = item
            .get("source_path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| blocked("guidance source missing"))?;
        if !applicable_source(path, guidance_scope, &scopes, &aliases) {
            return Err(blocked("rule source outside this chat's guidance scope"));
        }
        let source = call(&client, config, "scenario/read", json!({"path": path}))
            .await
            .map_err(blocked)?;
        if source.get("path").and_then(Value::as_str) != Some(path)
            || source.get("truncated").and_then(Value::as_bool) == Some(true)
        {
            return Err(blocked("guidance source mismatch or truncation"));
        }
        let content = source
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| blocked("guidance source unreadable"))?;
        let rule = json!({"scope":guidance_scope,"path":path,"content":content,
            "sha256":format!("{:x}",Sha256::digest(content.as_bytes()))});
        bytes += rule.to_string().len();
        if bytes > max_bytes {
            return Err(blocked("full guidance exceeds 256 KiB; consolidate active rules"));
        }
        rules.push(VerifiedSource {
            rule,
            record: item.clone(),
        });
    }
    // Never combine pages read from different indexed revisions. No cached success
    // is reused after a memory outage or a subsequent user turn.
    if inventory(&client, config, &scopes, &aliases, local_only, max_rules).await? != before {
        return Err(blocked("guidance changed during preflight; retry"));
    }
    rules.sort_by_key(|rule| {
        (
            rule.rule["scope"] != "global",
            rule.rule["path"].as_str().unwrap_or_default().to_owned(),
        )
    });
    if let (Some(cache), Some(key)) = (session_cache, cache_key) {
        *cache.lock().map_err(|_| blocked("guidance cache unavailable"))? = Some((key, rules.clone()));
    }
    Ok((
        project,
        rules,
        before.1.and_then(|(revision, writable)| writable.then_some(revision)),
    ))
}

#[cfg(test)]
pub(super) async fn load(
    config: &MemoryConfig,
    scope: &MemoryRecallScope,
    workspace: &str,
    local_only: bool,
) -> Result<String, crate::AgentError> {
    let snapshot = tool::read_snapshot(config, scope, workspace, local_only).await?;
    render_delivery(&snapshot, "full", snapshot["rules"].as_array().unwrap(), &[], true)
}

pub(crate) fn startup_rules_from_env(conversation_id: &str) -> Result<(), crate::AgentError> {
    let mut entry = SESSION_RECALL
        .get_mut(conversation_id)
        .ok_or_else(|| blocked("host project binding unavailable"))?;
    if entry.scope.isolated_test {
        return Ok(());
    }
    if load_config().is_none() {
        return Err(blocked("memory configuration unavailable"));
    }
    // A fresh or resumed worker has a different transcript. Defer all rule reads
    // to the actual tool invocation, and rehydrate on its first real task.
    entry.delivered_rules = Arc::default();
    Ok(())
}

#[cfg(test)]
pub(super) async fn required_rules_for_scope(
    config: &MemoryConfig,
    scope: &MemoryRecallScope,
    workspace: &str,
) -> Result<String, crate::AgentError> {
    tokio::time::timeout(Duration::from_secs(30), load(config, scope, workspace, false))
        .await
        .map_err(|_| blocked("guidance deadline exceeded"))?
}

/// Native children share the parent's authenticated host binding, not model-selected scope.
pub(crate) struct NativeChildRules {
    pub conversation_id: String,
    pub workspace: String,
}

#[async_trait::async_trait]
impl aion_agent::spawner::ChildPromptPreflight for NativeChildRules {
    async fn instructions(&self) -> Result<String, String> {
        let session = SESSION_RECALL
            .get(&self.conversation_id)
            .map(|s| s.clone())
            .ok_or_else(|| blocked("host project binding unavailable").to_string())?;
        let snapshot = invoke_rules_tool("tdai_rules_read", &json!({}), &session.scope, &self.workspace, false)
            .await
            .map_err(|e| e.to_string())?;
        let mut text = tool::render_delivery(&snapshot, "full", snapshot["rules"].as_array().unwrap(), &[], true)
            .map_err(|e| e.to_string())?;
        if let Some(skills) = super::project_skills_for_turn(&self.conversation_id, "", &self.workspace, vec![])
            .await
            .map_err(|e| e.to_string())?
        {
            text.push_str("\n\n");
            text.push_str(&skills);
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path},
    };

    pub(super) fn config(server: &MockServer) -> MemoryConfig {
        MemoryConfig {
            gateway_url: server.uri(),
            service_id: "fixture".into(),
            team_id: "team".into(),
            agent_id: "agent".into(),
            user_id: "user".into(),
            gateway_api_key: String::new(),
        }
    }
    pub(super) fn record(id: usize) -> Value {
        json!({"id":id.to_string(),"guidance_status":"active","source_status":"active","type":"instruction","guidance_scope":if id == 0 {"global"} else {"project"},"source_path":format!("knowledge/{}/rule-{id}.md", if id==0 {"global".to_owned()} else {project_scope("/tmp/fixture")})})
    }
    pub(super) fn page(items: Vec<Value>, total: usize, next: Option<usize>) -> Value {
        json!({"code":0,"data":{"guidance_inventory":true,"guidance_revision":"fixture-rev","guidance_write_version":1,"degraded":false,"returned":items.len(),"items":items,"total_candidates":total,"complete":next.is_none(),"next_offset":next}})
    }
    #[tokio::test]
    async fn loads_every_page_and_full_source_with_global_rules_first() {
        let server = MockServer::start().await;
        for (offset, end, next) in [(0, 16, Some(16)), (16, 19, None)] {
            Mock::given(method("POST"))
                .and(path("/v3/atomic/search"))
                .and(body_partial_json(
                    json!({"offset":offset,"guidance_inventory":true,"include_global":true}),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(page(
                    (offset..end).map(record).collect(),
                    19,
                    next,
                )))
                .expect(2)
                .mount(&server)
                .await;
        }
        for id in 0..19 {
            let path = record(id)["source_path"].as_str().unwrap().to_owned();
            Mock::given(method("POST")).and(path_match()).and(body_partial_json(json!({"path":path})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"path":path,"content":format!("{}FULL_END_{id}", "rule body ".repeat(200))}})))
                .expect(1).mount(&server).await;
        }
        let text = load(
            &config(&server),
            &MemoryRecallScope {
                project_id: Some("fixture".into()),
                ..Default::default()
            },
            "/tmp/fixture",
            false,
        )
        .await
        .unwrap();
        assert!(text.starts_with("<ellmsha_required_rules"));
        for id in 0..19 {
            assert!(text.contains(&format!("FULL_END_{id}")));
        }
        assert!(text.find("rule-0.md").unwrap() < text.find("rule-1.md").unwrap());
        server.verify().await;
    }
    fn path_match() -> impl wiremock::Match {
        path("/v3/scenario/read")
    }

    #[tokio::test]
    async fn distinct_records_for_the_same_active_source_fail_before_body_reads() {
        let server = MockServer::start().await;
        let first = record(0);
        let mut duplicate = first.clone();
        duplicate["id"] = json!("other-record");
        Mock::given(path("/v3/atomic/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![first, duplicate], 2, None)))
            .expect(1)
            .mount(&server)
            .await;
        let error = load(&config(&server), &MemoryRecallScope::default(), "/tmp/fixture", false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("same guidance source"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn rejects_incomplete_degraded_duplicate_unacknowledged_or_changed_pages() {
        for kind in ["degraded", "cursor", "duplicate", "count", "old-server", "changed"] {
            let server = MockServer::start().await;
            let mut body = page(vec![record(0)], 1, None);
            match kind {
                "degraded" => body["data"]["degraded"] = json!(true),
                "cursor" => {
                    body["data"]["complete"] = json!(false);
                    body["data"]["next_offset"] = json!(0);
                }
                "duplicate" => body = page(vec![record(0), record(0)], 2, None),
                "count" => body["data"]["total_candidates"] = json!(2),
                "old-server" => {
                    body["data"].as_object_mut().unwrap().remove("guidance_inventory");
                }
                "changed" => (),
                _ => unreachable!(),
            }
            let changed = kind == "changed";
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            Mock::given(method("POST"))
                .and(path("/v3/atomic/search"))
                .respond_with(move |_: &wiremock::Request| {
                    let mut value = body.clone();
                    if changed && calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                        value["data"]["items"][0]["version"] = json!(2);
                    }
                    ResponseTemplate::new(200).set_body_json(value)
                })
                .mount(&server)
                .await;
            Mock::given(path("/v3/scenario/read"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!({"code":0,"data":{"path":"knowledge/fixture/rule-0.md","content":"REQUIRED"}}),
                ))
                .mount(&server)
                .await;
            let error = load(&config(&server), &MemoryRecallScope::default(), "/tmp/fixture", false)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("task was not sent to the model"),
                "{kind}: {error}"
            );
        }
    }
    #[tokio::test]
    async fn rejects_missing_truncated_oversized_and_wrong_sources() {
        for kind in ["missing", "truncated", "oversized", "wrong"] {
            let server = MockServer::start().await;
            Mock::given(path("/v3/atomic/search"))
                .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![record(0)], 1, None)))
                .mount(&server)
                .await;
            let mut source = json!({"path":"knowledge/fixture/rule-0.md","content":"full rule"});
            match kind {
                "missing" => source["content"] = Value::Null,
                "truncated" => source["truncated"] = json!(true),
                "oversized" => source["content"] = json!("x".repeat(MAX_BYTES + 1)),
                "wrong" => source["path"] = json!("knowledge/foreign/rule.md"),
                _ => unreachable!(),
            }
            Mock::given(path("/v3/scenario/read"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":source})))
                .mount(&server)
                .await;
            assert!(
                load(&config(&server), &MemoryRecallScope::default(), "/tmp/fixture", false)
                    .await
                    .is_err(),
                "{kind}"
            );
        }
    }
    #[tokio::test]
    async fn local_only_requires_both_loopback_and_a_local_store_attestation() {
        let server = MockServer::start().await;
        let mut response = page(vec![], 0, None);
        response["data"]["guidance_locality"] = json!("unverified");
        Mock::given(path("/v3/atomic/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;
        let error = load(&config(&server), &MemoryRecallScope::default(), "/tmp/fixture", true)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("memory locality is unverified"));
        let mut remote = config(&server);
        remote.gateway_url = "https://example.invalid".into();
        let error = load(&remote, &MemoryRecallScope::default(), "/tmp/fixture", true)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("gateway is not loopback"));
    }

    #[test]
    fn guidance_failure_is_owned_by_host_not_agent_or_model_provider() {
        let error = crate::AgentSendError::from_agent_error(blocked("fixture outage"));
        assert_eq!(error.ownership(), Some(aionui_api_types::AgentErrorOwnership::Aionui));
        assert_eq!(error.stream_error().retryable, Some(true));
        assert_eq!(error.stream_error().feedback_recommended, Some(false));
    }

    #[tokio::test]
    async fn required_provider_preflight_refreshes_only_applicable_unmanaged_repository_rules() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/atomic/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![], 0, None)))
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().canonicalize().unwrap();
        let file = cwd.join("AGENTS.md");
        let text = |rule: &str| {
            format!(
                "<!-- BEGIN PROJECT-MEMORY MANAGED BLOCK -->\nHistorical BMAD specification\n<!-- END PROJECT-MEMORY MANAGED BLOCK -->\n# Local instructions\n\n{rule}\n"
            )
        };
        std::fs::write(&file, text("First instruction")).unwrap();
        let scope = MemoryRecallScope {
            project_id: Some("fixture-project".into()),
            roots: vec![cwd.display().to_string()],
            ..Default::default()
        };
        let first = required_rules_for_scope(&config(&server), &scope, cwd.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(first.matches("First instruction").count(), 1);
        assert!(!first.contains("Historical BMAD specification"));
        std::fs::write(&file, text("Second instruction")).unwrap();
        let fresh = required_rules_for_scope(&config(&server), &scope, cwd.to_str().unwrap())
            .await
            .unwrap();
        assert!(fresh.contains("Second instruction") && !fresh.contains("First instruction"));
    }

    #[tokio::test]
    async fn missing_host_binding_never_silently_skips_rules() {
        let error = startup_rules_from_env("not-a-registered-chat").unwrap_err();
        assert!(error.to_string().contains("host project binding unavailable"));
    }
}
