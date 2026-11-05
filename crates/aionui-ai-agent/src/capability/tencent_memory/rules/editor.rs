//! Structured editing of the same sources verified by the mandatory prompt barrier.
use super::*;
use aionui_api_types::{ChatRule, ChatRuleMutation, ChatRuleScope, ChatRulesResponse};

const IMPORT_START: &str = "<!-- ELLMSHA_V2_SOURCE_START -->";
const IMPORT_END: &str = "<!-- ELLMSHA_V2_SOURCE_END -->";

/// Keep the scenario envelope, imported provenance and frontmatter out of the form.
struct Fields<'a> {
    prefix: &'a str,
    suffix: &'a str,
    frontmatter_start: Option<usize>,
    newline: &'static str,
    title: String,
    body: &'a str,
}

fn fields<'a>(source: &'a str, path: &str) -> Fields<'a> {
    let (meta, document) = source
        .strip_prefix("-----META-START-----\n")
        .and_then(|s| s.split_once("\n-----META-END-----\n"))
        .map_or(("", source), |(meta, rest)| {
            (
                meta,
                rest.strip_prefix("\r\n")
                    .or_else(|| rest.strip_prefix('\n'))
                    .unwrap_or(rest),
            )
        });
    let (start, note, end) = path
        .starts_with("v2/")
        .then(|| document.split_once(IMPORT_START))
        .flatten()
        .and_then(|(before, inner)| {
            inner
                .rsplit_once(IMPORT_END)
                .map(|(note, after)| (before.len() + IMPORT_START.len(), note, after.len() + IMPORT_END.len()))
        })
        .unwrap_or((0, document, 0));
    let content = note
        .strip_prefix("---\n")
        .or_else(|| note.strip_prefix("---\r\n"))
        .and_then(|rest| {
            let mut offset = 0;
            for line in rest.split_inclusive('\n') {
                offset += line.len();
                if line.trim_end_matches(['\r', '\n']) == "---" {
                    return Some(&rest[offset..]);
                }
            }
            None
        })
        .unwrap_or(note);
    let prefix = &document[..start + note.len() - content.len()];
    let suffix = &document[document.len() - end..];
    let body = content.trim_start_matches(['\r', '\n']);
    let heading = body
        .strip_prefix("# ")
        .map(|header| header.split_once('\n').unwrap_or((header, "")));
    let title = heading
        .map(|(title, _)| title.trim())
        .or_else(|| {
            meta.lines()
                .find_map(|line| line.strip_prefix("summary: "))
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).trim_end_matches(".md"))
        .to_owned();
    Fields {
        prefix,
        suffix,
        frontmatter_start: (content.len() != note.len()).then_some(start),
        newline: if note.contains("\r\n") { "\r\n" } else { "\n" },
        title,
        body: heading.map_or(body, |(_, rest)| {
            rest.strip_prefix("\r\n")
                .or_else(|| rest.strip_prefix('\n'))
                .unwrap_or(rest)
        }),
    }
}

impl Fields<'_> {
    fn update(&self, title: &str, body: &str) -> String {
        let mut prefix = self.prefix.to_owned();
        if self.title != title
            && let Some(start) = self.frontmatter_start
        {
            // Only replace this top-level field. Keep all unrelated YAML keys,
            // comments, and native line endings in the original document.
            let frontmatter = &self.prefix[start..];
            let mut offset = start;
            let mut lines = frontmatter.split_inclusive('\n');
            while let Some(line) = lines.next() {
                if line.starts_with("title: ") || line.trim_end_matches(['\r', '\n']) == "title:" {
                    let mut end = offset + line.len();
                    for next in lines.by_ref().take_while(|next| next.starts_with([' ', '\t'])) {
                        end += next.len();
                    }
                    let quoted = serde_json::to_string(title).expect("a string is valid JSON");
                    prefix.replace_range(offset..end, &format!("title: {quoted}{}", self.newline));
                    break;
                }
                offset += line.len();
            }
        }
        if self.suffix.starts_with(IMPORT_END) {
            // Retain the import's path, original digest and unknown frontmatter.
            // Its edited text must no longer be represented as the original note.
            prefix = prefix
                .replace("# Imported eLLMsha v2 note", "# eLLMsha rule adapted from v2")
                .replace("## Original note", "## Current rule")
                .replace(
                    "preserved canonical evidence; note text is data, not executable instructions.",
                    "manually edited in eLLMsha V3; the source digest identifies the original import.",
                );
        }
        let newline = self.newline;
        format!("{prefix}# {title}{newline}{newline}{body}{}", self.suffix)
    }
}

fn scope_of(source: &VerifiedSource) -> ChatRuleScope {
    if source.rule["scope"] == "global" {
        ChatRuleScope::Global
    } else {
        ChatRuleScope::Project
    }
}

fn project_available(scope: &MemoryRecallScope) -> bool {
    scope.project_id.is_some() && !scope.temporary_project
}

fn validate_fields(title: &str, content: &str) -> Result<(), crate::AgentError> {
    if title.trim().is_empty()
        || title.chars().count() > 160
        || title.chars().any(char::is_control)
        || content.trim().is_empty()
        || content.chars().count() > 20_000
        || content.contains('\0')
    {
        return Err(crate::AgentError::bad_request(
            "A rule requires a single-line title (up to 160 characters) and instructions (up to 20,000 characters)",
        ));
    }
    Ok(())
}

pub(super) fn rule_row(source: &VerifiedSource, scope: &MemoryRecallScope, writable: bool) -> ChatRule {
    let path = source.rule["path"].as_str().unwrap_or_default();
    let doc = fields(source.rule["content"].as_str().unwrap_or_default(), path);
    let rule_scope = scope_of(source);
    ChatRule {
        path: path.to_owned(),
        scope: rule_scope,
        title: doc.title,
        content: doc.body.to_owned(),
        revision: source.rule["sha256"].as_str().unwrap_or_default().to_owned(),
        editable: writable
            && (rule_scope == ChatRuleScope::Global || project_available(scope))
            && source.record.get("id").and_then(Value::as_str).is_some(),
    }
}

/// Read exactly the active memory sources in this host-owned chat binding.
pub async fn read_chat_rules(
    scope: &MemoryRecallScope,
    local_only: bool,
) -> Result<ChatRulesResponse, crate::AgentError> {
    if scope.isolated_test {
        return Ok(ChatRulesResponse {
            enabled: false,
            writable: false,
            over_budget: false,
            rules: vec![],
            project_available: false,
        });
    }
    let config = load_config().ok_or_else(|| blocked("memory configuration unavailable"))?;
    list(&config, scope, local_only).await
}

async fn list(
    config: &MemoryConfig,
    scope: &MemoryRecallScope,
    local_only: bool,
) -> Result<ChatRulesResponse, crate::AgentError> {
    let (project, sources, write_revision) = tokio::time::timeout(
        Duration::from_secs(30),
        read_sources(config, scope, &scope.workspace, local_only, true),
    )
    .await
    .map_err(|_| blocked("guidance deadline exceeded"))??;
    let writable = write_revision.is_some();
    Ok(ChatRulesResponse {
        enabled: true,
        writable,
        over_budget: sources.len() > MAX_RULES
            || guidance_size(&project, &sources.iter().map(|s| s.rule.clone()).collect::<Vec<_>>())? > MAX_BYTES,
        project_available: project_available(scope),
        rules: sources.iter().map(|s| rule_row(s, scope, writable)).collect(),
    })
}

fn guidance_size(project: &str, rules: &[Value]) -> Result<usize, crate::AgentError> {
    let rows: Vec<_> = rules
        .iter()
        .map(|r| {
            let path = r["path"].as_str().unwrap_or_default();
            let doc = fields(r["content"].as_str().unwrap_or_default(), path);
            json!({"scope":r["scope"],"path":path,"revision":r["sha256"],"title":doc.title,"content":doc.body})
        })
        .collect();
    let meta = json!({"project":project,"revision":"0".repeat(64)});
    Ok(tool::delivery_text(&meta, "full", &rows, &[], true)?
        .len()
        .max(
            serde_json::to_vec(&(Some(project), &rows))
                .map_err(|_| blocked("invalid rule snapshot"))?
                .len(),
        )
        .max(rules.iter().map(|r| r.to_string().len()).sum()))
}

fn check_budget(project: &str, sources: &[VerifiedSource], request: &Value) -> Result<(), crate::AgentError> {
    if request["action"] == "delete" {
        return Ok(());
    }
    let mut rules: Vec<_> = sources.iter().map(|s| s.rule.clone()).collect();
    let before = guidance_size(project, &rules)?;
    let old = rules.iter().find(|r| r["path"] == request["path"]);
    let old_meta = old
        .and_then(|r| r["content"].as_str())
        .and_then(|body| body.strip_prefix("-----META-START-----\n"))
        .and_then(|body| body.split_once("\n-----META-END-----"))
        .map(|(meta, _)| meta);
    // Match the gateway's bounded ISO timestamp lengths. Preserve foreign metadata
    // in this size estimate, including escaped text that counts towards the manifest.
    let timestamp = "0000-00-00T00:00:00.000Z";
    let meta = old_meta.map_or_else(
        || format!("created: {timestamp}\n"),
        |meta| {
            meta.lines()
                .filter(|line| !line.starts_with("updated: ") && !line.starts_with("summary: "))
                .map(|line| format!("{line}\n"))
                .collect()
        },
    );
    let raw = format!(
        "-----META-START-----\n{meta}updated: {timestamp}\nsummary: {}\n-----META-END-----\n\n{}",
        request["summary"].as_str().unwrap_or_default(),
        request["content"].as_str().unwrap_or_default()
    );
    let new = json!({"scope":old.map_or_else(||if request["path"].as_str().is_some_and(|p|p.starts_with("knowledge/global/")) {"global"} else {"project"}, |r|r["scope"].as_str().unwrap_or_default()),
        "path":request["path"],"content":raw,"sha256":"0".repeat(64)});
    if let Some(existing) = rules.iter_mut().find(|r| r["path"] == request["path"]) {
        *existing = new;
    } else {
        rules.push(new);
    }
    let after = guidance_size(project, &rules)?;
    if after > MAX_BYTES && after >= before {
        return Err(crate::AgentError::bad_request(
            "The active rules would exceed the 256 KiB prompt limit. Shorten or delete a rule before saving.",
        ));
    }
    Ok(())
}

/// Apply a manual edit only to an active source visible in this chat. The
/// gateway checks the revision again *inside* the scenario writer's critical section.
pub async fn mutate_chat_rule(
    scope: &MemoryRecallScope,
    mutation: &ChatRuleMutation,
    local_only: bool,
) -> Result<(), crate::AgentError> {
    if scope.isolated_test {
        return Err(crate::AgentError::forbidden(
            "Memory is unavailable in this isolated chat",
        ));
    }
    let config = load_config().ok_or_else(|| blocked("memory configuration unavailable"))?;
    tokio::time::timeout(Duration::from_secs(30), mutate(&config, scope, mutation, local_only))
        .await
        .map_err(|_| crate::AgentError::timeout("Memory rule update timed out; refresh the rules before retrying"))?
}

async fn mutate(
    config: &MemoryConfig,
    scope: &MemoryRecallScope,
    mutation: &ChatRuleMutation,
    local_only: bool,
) -> Result<(), crate::AgentError> {
    use ChatRuleMutation::{Add, Delete, Edit};
    let (project, sources, revision) = read_sources(config, scope, &scope.workspace, local_only, true).await?;
    let inventory_revision = revision
        .ok_or_else(|| crate::AgentError::bad_gateway("This memory gateway does not support safe rule editing"))?;
    let requested_scope = match mutation {
        Add { scope, .. } | Edit { scope, .. } | Delete { scope, .. } => *scope,
    };
    if requested_scope == ChatRuleScope::Project && !project_available(scope) {
        return Err(crate::AgentError::forbidden(
            "Project rules require a registered project",
        ));
    }
    let mut request = match mutation {
        Add { title, content, .. } => {
            validate_fields(title, content)?;
            if sources.len() >= MAX_RULES {
                return Err(crate::AgentError::bad_request("Too many active rules"));
            }
            let subject = slug(title.trim());
            let subject = if subject.is_empty() {
                format!("rule-{}", &hex::encode(Sha256::digest(title.trim().as_bytes()))[..12])
            } else {
                subject
                    .chars()
                    .take(120)
                    .collect::<String>()
                    .trim_end_matches('-')
                    .to_owned()
            };
            let prefix = if requested_scope == ChatRuleScope::Global {
                "global"
            } else {
                &project
            };
            let path = format!("knowledge/{prefix}/{subject}.md");
            json!({"action":"write","path":path,"expected_sha256":null,"summary":title.trim(),"content":format!("# {}\n\n{}",title.trim(),content),"index_content":format!("{}\n\n{}", title.trim(),content).chars().take(4000).collect::<String>(),"index_type":"instruction"})
        }
        Edit { path, revision, .. } | Delete { path, revision, .. } => {
            let mut matches = sources
                .iter()
                .filter(|s| s.rule["path"] == path.as_str() && scope_of(s) == requested_scope);
            let source = matches.next().ok_or_else(|| {
                crate::AgentError::conflict("This rule changed or is no longer active; refresh the rules")
            })?;
            if matches.next().is_some() || source.rule["sha256"] != revision.as_str() {
                return Err(crate::AgentError::conflict("This rule changed; refresh the rules"));
            }
            let mut req = json!({"path":path,"expected_sha256":revision,"source_id":source.record["id"]});
            if let Edit { title, content, .. } = mutation {
                validate_fields(title, content)?;
                let doc = fields(source.rule["content"].as_str().unwrap_or_default(), path);
                req["action"] = json!("write");
                req["content"] = json!(doc.update(title.trim(), content));
                req["summary"] = json!(title.trim());
                req["index_type"] = source.record["type"].clone();
                req["index_content"] = json!(
                    format!("{}\n\n{}", title.trim(), content)
                        .chars()
                        .take(4000)
                        .collect::<String>()
                );
            } else {
                req["action"] = json!("delete");
            }
            req
        }
    };
    check_budget(&project, &sources, &request)?;
    request["local_only"] = json!(local_only);
    request["expected_inventory"] = json!(inventory_revision);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| blocked("client unavailable"))?;
    call(&client, config, "scenario/rule-mutate", request)
        .await
        .map_err(|reason| {
            if reason == "conflict" {
                crate::AgentError::conflict("This rule changed or already exists; refresh the rules before saving")
            } else {
                crate::AgentError::bad_gateway("Memory could not save this rule; refresh the rules before retrying")
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path},
    };

    #[test]
    fn imported_fields_keep_unknown_frontmatter_and_original_provenance() {
        let source = "-----META-START-----\nsummary: note\n-----META-END-----\n\n# Imported eLLMsha v2 note\n- Source SHA-256: original\n## Original note\n<!-- ELLMSHA_V2_SOURCE_START -->---\ntitle: old\nother: untouched\n---\n\n# Old title\n\nFollow the project rule.<!-- ELLMSHA_V2_SOURCE_END -->";
        let doc = fields(source, "v2/wiki/test.md");
        assert_eq!((&doc.title[..], doc.body), ("Old title", "Follow the project rule."));
        let updated = doc.update("New title", "Revised instruction.");
        assert!(updated.contains("- Source SHA-256: original"));
        assert!(updated.contains("title: \"New title\"\nother: untouched"));
        assert!(!updated.contains("## Original note"));
        let result = fields(&updated, "v2/wiki/test.md");
        assert_eq!((&result.title[..], result.body), ("New title", "Revised instruction."));
        assert!(!updated.contains("META-START"));
    }

    #[test]
    fn imported_crlf_frontmatter_is_preserved_without_exposing_it_as_the_rule() {
        let raw = "metadata source\n<!-- ELLMSHA_V2_SOURCE_START -->---\r\ntitle: Before\r\nx-fixture: preserved\r\n---\r\n\r\n# Before\r\n\r\nUse fixtures.<!-- ELLMSHA_V2_SOURCE_END -->";
        let doc = fields(raw, "v2/wiki/fixture.md");
        assert_eq!(doc.body, "Use fixtures.");
        let updated = doc.update("After", "Keep fixtures.");
        assert!(updated.contains("title: \"After\"\r\nx-fixture: preserved\r\n---\r\n# After\r\n\r\nKeep fixtures."));
    }

    #[test]
    fn prompt_budget_blocks_growth_and_allows_repair() {
        let raw = "x".repeat(MAX_BYTES);
        let path = "knowledge/global/budget.md";
        let sources = vec![VerifiedSource {
            rule: json!({"path":path,"scope":"global","content":raw,"sha256":"0".repeat(64)}),
            record: json!({}),
        }];
        let req = |body: &str| json!({"action":"write","path":path,"content":body,"summary":"budget"});
        assert!(check_budget("test", &sources, &req(&"a".repeat(MAX_BYTES + 1))).is_err());
        assert!(check_budget("test", &sources, &req("Shorter rule")).is_ok());
        assert!(check_budget("test", &sources, &json!({"action":"delete","path":path})).is_ok());
    }

    #[tokio::test]
    async fn inventory_remains_readable_and_only_explicit_supported_gates_advertise_edits() {
        for (version, revision, writable) in [
            (None, Some("old-revision"), false),
            (Some(0), Some("remote-revision"), false),
            (Some(1), None, false),
            (Some(1), Some("native-revision"), true),
        ] {
            let server = MockServer::start().await;
            let mut response = super::super::tests::page(vec![super::super::tests::record(0)], 1, None);
            response["data"]["guidance_write_version"] = json!(version);
            response["data"]["guidance_revision"] = json!(revision);
            Mock::given(path("/v3/atomic/search"))
                .respond_with(ResponseTemplate::new(200).set_body_json(response))
                .mount(&server)
                .await;
            let source = "knowledge/global/rule-0.md";
            Mock::given(path("/v3/scenario/read"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"code":0,"data":{"path":source,"content":"# Keep a test\n\nRun it."}})),
                )
                .mount(&server)
                .await;
            let config = super::super::tests::config(&server);
            let scope = MemoryRecallScope::default();
            let result = list(&config, &scope, false).await.unwrap();
            assert!(result.enabled && result.rules.len() == 1);
            assert_eq!(result.rules[0].content, "Run it.");
            assert_eq!(result.writable, writable);
            assert_eq!(result.rules[0].editable, writable);
            if !writable {
                let change = ChatRuleMutation::Delete {
                    scope: ChatRuleScope::Global,
                    path: source.into(),
                    revision: result.rules[0].revision.clone(),
                };
                let error = mutate(&config, &scope, &change, false).await.unwrap_err();
                assert!(error.to_string().contains("does not support safe rule editing"));
            }
        }
    }

    #[tokio::test]
    async fn temporary_workspace_under_git_requests_only_global_rules_and_rejects_foreign_results() {
        let server = MockServer::start().await;
        let scope = MemoryRecallScope {
            project_id: Some("temp-record".into()),
            temporary_project: true,
            project_name: Some("OtherProject".into()),
            roots: vec![env!("CARGO_MANIFEST_DIR").into()],
            ..Default::default()
        };
        let workspace = env!("CARGO_MANIFEST_DIR");
        Mock::given(path("/v3/atomic/search")).and(body_partial_json(json!({"scope_mode":"current_project","guidance_inventory":true,"project_scopes":[],"project_aliases":[],"include_global":true})))
            .respond_with(ResponseTemplate::new(200).set_body_json(super::super::tests::page(vec![super::super::tests::record(1)],1,None))).expect(1).mount(&server).await;
        let error = load(&super::super::tests::config(&server), &scope, workspace, false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("outside this chat"));
        server.verify().await;
    }

    #[tokio::test]
    async fn edit_uses_verified_source_identity_kind_and_full_source_precondition() {
        let server = MockServer::start().await;
        let rule_path = "knowledge/global/safety.md";
        let raw = "-----META-START-----\nsummary: safe\n-----META-END-----\n\n# Title\n\nOld instruction.";
        let revision = format!("{:x}", Sha256::digest(raw.as_bytes()));
        let rec = json!({"id":"existing","guidance_status":"active","source_status":"active","type":"persona","guidance_scope":"global","source_path":rule_path});
        Mock::given(path("/v3/atomic/search"))
            .and(body_partial_json(
                json!({"guidance_inventory":true,"include_global":true}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(super::super::tests::page(vec![rec], 1, None)))
            .mount(&server)
            .await;
        Mock::given(path("/v3/scenario/read"))
            .and(body_partial_json(json!({"path":rule_path})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"path":rule_path,"content":raw}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/v3/scenario/rule-mutate")).and(body_partial_json(json!({"path":rule_path,"expected_sha256":revision,"index_type":"persona","source_id":"existing","content":"# New title\n\nUpdated instruction"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"memory_id":"existing"}}))).expect(1).mount(&server).await;
        let config = super::super::tests::config(&server);
        let scope = MemoryRecallScope::default();
        let change = |path: &str, rev: &str| ChatRuleMutation::Edit {
            scope: ChatRuleScope::Global,
            path: path.into(),
            revision: rev.into(),
            title: "New title".into(),
            content: "Updated instruction".into(),
        };
        mutate(&config, &scope, &change(rule_path, &revision), false)
            .await
            .unwrap();
        for (path, rev) in [(rule_path, "stale"), ("knowledge/other/foreign.md", &revision)] {
            assert!(matches!(
                mutate(&config, &scope, &change(path, rev), false).await,
                Err(crate::AgentError::Conflict(_))
            ));
        }
        server.verify().await;
    }
}
