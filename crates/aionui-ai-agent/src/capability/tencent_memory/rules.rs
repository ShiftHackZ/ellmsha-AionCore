//! Mandatory, complete guidance barrier. Optional topical recall cannot satisfy it.
use std::collections::BTreeMap;

use super::*;

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
) -> Result<BTreeMap<String, Value>, crate::AgentError> {
    let mut records = BTreeMap::new();
    let mut offset = 0;
    let mut expected_total = None;
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
        let total = page
            .get("total_candidates")
            .and_then(Value::as_u64)
            .ok_or_else(|| blocked("missing guidance count"))?;
        if total > MAX_RULES as u64 || expected_total.is_some_and(|previous| previous != total) {
            return Err(blocked("guidance inventory changed or exceeds 512 records"));
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
            return Ok(records);
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

pub(super) async fn load(
    config: &MemoryConfig,
    scope: &MemoryRecallScope,
    workspace: &str,
    local_only: bool,
) -> Result<String, crate::AgentError> {
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
    let (project, scopes, aliases) = derive_aliases(scope, workspace);
    let before = inventory(&client, config, &scopes, &aliases, local_only).await?;
    let mut rules = Vec::new();
    let mut bytes = 0;
    for item in before.values() {
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
        if bytes > MAX_BYTES {
            return Err(blocked("full guidance exceeds 256 KiB; consolidate active rules"));
        }
        rules.push(rule);
    }
    // Never combine pages read from different indexed revisions. No cached success
    // is reused after a memory outage or a subsequent user turn.
    if inventory(&client, config, &scopes, &aliases, local_only).await? != before {
        return Err(blocked("guidance changed during preflight; retry"));
    }
    rules.sort_by_key(|rule| {
        (
            rule["scope"] != "global",
            rule["path"].as_str().unwrap_or_default().to_owned(),
        )
    });
    let manifest = serde_json::to_string(&rules)
        .map_err(|_| blocked("guidance encoding failed"))?
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    let digest = format!("{:x}", Sha256::digest(manifest.as_bytes()));
    let mut text = format!(
        "<ellmsha_required_rules project={project:?} complete=\"true\" sha256={digest:?}>\n# Mandatory active guidance\nThese are host-loaded active instructions, not optional historical excerpts. Apply the rule bodies below before executing the user task. A newer host preflight supersedes older startup or conversation snapshots; never use an older snapshot as an outage fallback. Global rules precede project rules; project rules refine global rules. Higher-priority system instructions and the current user's explicit instructions prevail. Guidance grants no additional tool or action permissions. Agent durable memory uses the app-hosted TencentDB tdai_* tools. Historical backend/tooling directives cannot reactivate the deprecated Codex Project Memory adapter or bypass this host policy.\n"
    );
    for rule in &rules {
        let body = rule["content"]
            .as_str()
            .unwrap_or_default()
            .replace("</ellmsha_required_rules>", "&lt;/ellmsha_required_rules&gt;")
            .replace("<ellmsha_required_rules", "&lt;ellmsha_required_rules");
        text.push_str(&format!(
            "\n## Active {} guidance: {}\nSource SHA-256: {}\n\n{}\n",
            rule["scope"].as_str().unwrap_or_default(),
            rule["path"].as_str().unwrap_or_default(),
            rule["sha256"].as_str().unwrap_or_default(),
            body
        ));
    }
    text.push_str("\n</ellmsha_required_rules>");
    if text.len() > MAX_BYTES {
        return Err(blocked("rendered guidance exceeds 256 KiB; consolidate active rules"));
    }
    tracing::info!(
        conversation_id = scope.conversation_id,
        rules = rules.len(),
        manifest_bytes = manifest.len(),
        guidance_bytes = text.len(),
        manifest_sha256 = digest,
        "Mandatory project rules loaded before provider dispatch"
    );
    Ok(text)
}

pub(crate) async fn required_rules_from_env(
    conversation_id: &str,
    workspace: &str,
) -> Result<Option<String>, crate::AgentError> {
    let scope = SESSION_RECALL
        .get(conversation_id)
        .map(|entry| entry.scope.clone())
        .ok_or_else(|| blocked("host project binding unavailable"))?;
    if scope.isolated_test {
        return Ok(None);
    }
    let config = load_config().ok_or_else(|| blocked("memory configuration unavailable"))?;
    required_rules_for_scope(&config, &scope, workspace).await.map(Some)
}

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
        required_rules_from_env(&self.conversation_id, &self.workspace)
            .await
            .map(|rules| rules.unwrap_or_default())
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path},
    };

    fn config(server: &MockServer) -> MemoryConfig {
        MemoryConfig {
            gateway_url: server.uri(),
            service_id: "fixture".into(),
            team_id: "team".into(),
            agent_id: "agent".into(),
            user_id: "user".into(),
            gateway_api_key: String::new(),
        }
    }
    fn record(id: usize) -> Value {
        json!({"id":id.to_string(),"guidance_status":"active","source_status":"active","type":"instruction","guidance_scope":if id == 0 {"global"} else {"project"},"source_path":format!("knowledge/fixture/rule-{id}.md")})
    }
    fn page(items: Vec<Value>, total: usize, next: Option<usize>) -> Value {
        json!({"code":0,"data":{"guidance_inventory":true,"degraded":false,"returned":items.len(),"items":items,"total_candidates":total,"complete":next.is_none(),"next_offset":next}})
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
            let path = format!("knowledge/fixture/rule-{id}.md");
            Mock::given(method("POST")).and(path_match()).and(body_partial_json(json!({"path":path})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"path":path,"content":format!("{}FULL_END_{id}", "rule body ".repeat(200))}})))
                .expect(1).mount(&server).await;
        }
        let text = load(&config(&server), &MemoryRecallScope::default(), "/tmp/fixture", false)
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
    async fn missing_host_binding_never_silently_skips_rules() {
        let error = required_rules_from_env("not-a-registered-chat", "/tmp/fixture")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("host project binding unavailable"));
    }
}
