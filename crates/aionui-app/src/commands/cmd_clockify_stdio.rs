//! `aioncore mcp-clockify-stdio`: one MCP server per verified Clockify identity.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Weak};
use std::time::Duration;

use aionui_api_types::ClockifyRealm;
use aionui_extension::{
    CLOCKIFY_ACCOUNT_EMAIL_ENV, CLOCKIFY_ACCOUNT_ID_ENV, CLOCKIFY_ACCOUNT_NAME_ENV, CLOCKIFY_API_KEY_ENV,
    CLOCKIFY_CONNECTION_ID_ENV, CLOCKIFY_LOCK_DIR_ENV, CLOCKIFY_REALM_ENV, clockify_api_base, realm_name,
};
use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use fs2::FileExt;
use reqwest::{Method, StatusCode};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{schemars, service::ServiceExt, tool, tool_router, transport};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

const MAX_PROVIDER_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROVIDER_ERROR_BYTES: usize = 16 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_REPORT_REQUESTS: usize = 20;
const MAX_FETCHED_ENTRIES: usize = 5_000;
const MAX_EXPOSED_ENTRIES: usize = 500;
const MAX_EXPOSED_GROUPS: usize = 500;
const REPORT_PAGE_SIZE: usize = 200;
const REPORT_DEADLINE: Duration = Duration::from_secs(60);
const MAX_CUSTOM_FIELDS_BYTES: usize = 256 * 1024;
const MAX_CUSTOM_FIELD_STRING_BYTES: usize = 16 * 1024;

pub async fn run_clockify_stdio() -> ExitCode {
    let server = match ClockifyStdioServer::from_env() {
        Ok(server) => server,
        Err(message) => {
            eprintln!("CLOCKIFY_MCP_CONFIG_INVALID: {message}");
            return ExitCode::from(2);
        }
    };
    match server.serve(transport::io::stdio()).await {
        Ok(peer) => match peer.waiting().await {
            Ok(_) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        },
        Err(_) => ExitCode::from(1),
    }
}

#[derive(Clone)]
struct ClockifyStdioServer {
    client: ClockifyClient,
}

impl ClockifyStdioServer {
    fn from_env() -> Result<Self, String> {
        let api_key = required_env(CLOCKIFY_API_KEY_ENV)?;
        if api_key.len() > 16_384 {
            return Err("API key exceeds the safe size limit".into());
        }
        let realm = parse_realm(&required_env(CLOCKIFY_REALM_ENV)?)?;
        let account_id = provider_id(&required_env(CLOCKIFY_ACCOUNT_ID_ENV)?)?;
        let account_name = bounded_text(&required_env(CLOCKIFY_ACCOUNT_NAME_ENV)?, 300, "account name")?;
        let account_email = std::env::var(CLOCKIFY_ACCOUNT_EMAIL_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| bounded_text(&value, 320, "account email"))
            .transpose()?;
        let connection_id = bounded_text(&required_env(CLOCKIFY_CONNECTION_ID_ENV)?, 300, "connection ID")?;
        let mutation_lock_dir = private_directory_env(CLOCKIFY_LOCK_DIR_ENV)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "HTTP client could not be initialized".to_string())?;
        Ok(Self {
            client: ClockifyClient {
                http,
                api_base: clockify_api_base(realm).to_owned(),
                reports_base: reports_base(realm).to_owned(),
                api_key,
                account_id,
                account_name,
                account_email,
                connection_id,
                realm: realm_name(realm).into(),
                mutation_lock_dir,
                mutation_locks: Arc::new(Mutex::new(HashMap::new())),
            },
        })
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListProjectsParams {
    /// Workspace identifier returned by clockify_list_workspaces.
    workspace_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetTimeEntryParams {
    workspace_id: String,
    time_entry_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum GroupBy {
    Day,
    Task,
    Description,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ReportDetail {
    None,
    Summary,
    Full,
}

fn default_group_by() -> GroupBy {
    GroupBy::Day
}

fn default_detail() -> ReportDetail {
    ReportDetail::Summary
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetTimeReportParams {
    workspace_id: String,
    project_id: String,
    /// Inclusive RFC 3339 start with an explicit offset.
    #[schemars(length(max = 100))]
    start: String,
    /// Exclusive RFC 3339 end with an explicit offset, at most 366 days after start.
    #[schemars(length(max = 100))]
    end: String,
    /// IANA timezone used by Clockify and day grouping.
    #[schemars(length(max = 100))]
    time_zone: String,
    #[serde(default = "default_group_by")]
    group_by: GroupBy,
    #[serde(default = "default_detail")]
    detail: ReportDetail,
    #[serde(default)]
    include_running: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TimeEntryCore {
    /// RFC 3339 timestamp with an explicit offset.
    #[schemars(length(max = 100))]
    start: String,
    /// RFC 3339 timestamp with an explicit offset, after start.
    #[schemars(length(max = 100))]
    end: String,
    #[schemars(length(min = 1, max = 3000))]
    description: String,
    billable: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExpectedTimeEntryCore {
    /// RFC 3339 timestamp with an explicit offset.
    #[schemars(length(max = 100))]
    start: String,
    /// RFC 3339 timestamp with an explicit offset, after start.
    #[schemars(length(max = 100))]
    end: String,
    /// The exact prior description. Existing Clockify entries may have no description.
    #[schemars(length(max = 3000))]
    description: String,
    billable: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateTimeEntryParams {
    workspace_id: String,
    project_id: String,
    #[serde(flatten)]
    entry: TimeEntryCore,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateTimeEntryParams {
    workspace_id: String,
    project_id: String,
    time_entry_id: String,
    /// Exact snapshot previously returned by clockify_get_time_entry.
    expected: ExpectedTimeEntryCore,
    replacement: TimeEntryCore,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteTimeEntryParams {
    workspace_id: String,
    project_id: String,
    time_entry_id: String,
    /// Exact snapshot previously returned by clockify_get_time_entry.
    expected: ExpectedTimeEntryCore,
}

#[tool_router]
impl ClockifyStdioServer {
    #[tool(
        name = "clockify_list_workspaces",
        description = "List the bounded Clockify workspaces visible to this verified account.",
        annotations(title = "List Clockify workspaces", read_only_hint = true, open_world_hint = true)
    )]
    async fn list_workspaces(&self) -> CallToolResult {
        tool_from_result(self.client.list_workspaces().await)
    }

    #[tool(
        name = "clockify_list_projects",
        description = "List up to 500 active Clockify projects in a chosen provider workspace.",
        annotations(title = "List Clockify projects", read_only_hint = true, open_world_hint = true)
    )]
    async fn list_projects(&self, Parameters(params): Parameters<ListProjectsParams>) -> CallToolResult {
        tool_from_result(self.client.list_projects(params).await)
    }

    #[tool(
        name = "clockify_get_time_report",
        description = "Read a bounded personal Clockify report for a provider workspace/project selected for this operation. Returns explicit completeness and partial-reason fields.",
        annotations(title = "Read Clockify time report", read_only_hint = true, open_world_hint = true)
    )]
    async fn get_time_report(&self, Parameters(params): Parameters<GetTimeReportParams>) -> CallToolResult {
        tool_from_result(self.client.get_time_report(params).await)
    }

    #[tool(
        name = "clockify_get_time_entry",
        description = "Read one Clockify time entry for preflight inspection before an update or delete.",
        annotations(title = "Read Clockify time entry", read_only_hint = true, open_world_hint = true)
    )]
    async fn get_time_entry(&self, Parameters(params): Parameters<GetTimeEntryParams>) -> CallToolResult {
        tool_from_result(
            self.client
                .get_time_entry(params)
                .await
                .map(|entry| self.client.account_result(serde_json::json!({ "entry": entry }))),
        )
    }

    #[tool(
        name = "clockify_create_time_entry",
        description = "Create one completed Clockify time entry. This external write requires explicit user authorization.",
        annotations(
            title = "Create Clockify time entry",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn create_time_entry(&self, Parameters(params): Parameters<CreateTimeEntryParams>) -> CallToolResult {
        tool_from_result(self.client.create_time_entry(params).await)
    }

    #[tool(
        name = "clockify_update_time_entry",
        description = "Update one Clockify time entry only when its exact current snapshot matches expected. This external write requires explicit user authorization.",
        annotations(
            title = "Update Clockify time entry",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn update_time_entry(&self, Parameters(params): Parameters<UpdateTimeEntryParams>) -> CallToolResult {
        tool_from_result(self.client.update_time_entry(params).await)
    }

    #[tool(
        name = "clockify_delete_time_entry",
        description = "Delete one Clockify time entry only when its exact current snapshot matches expected. This irreversible external write requires explicit user authorization.",
        annotations(
            title = "Delete Clockify time entry",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn delete_time_entry(&self, Parameters(params): Parameters<DeleteTimeEntryParams>) -> CallToolResult {
        tool_from_result(self.client.delete_time_entry(params).await)
    }
}

#[rmcp::tool_handler(router = Self::tool_router())]
impl rmcp::ServerHandler for ClockifyStdioServer {}

type MutationLocks = HashMap<(String, String), Weak<Mutex<()>>>;

#[derive(Clone)]
struct ClockifyClient {
    http: reqwest::Client,
    api_base: String,
    reports_base: String,
    api_key: String,
    account_id: String,
    account_name: String,
    account_email: Option<String>,
    connection_id: String,
    realm: String,
    mutation_lock_dir: PathBuf,
    mutation_locks: Arc<Mutex<MutationLocks>>,
}

#[derive(Debug)]
enum ClockifyError {
    Definite(String),
    DeliveryUncertain,
}

impl ClockifyClient {
    async fn list_workspaces(&self) -> Result<serde_json::Value, ClockifyError> {
        let value = self
            .request_json(ApiKind::Api, Method::GET, "/workspaces", None, false)
            .await?;
        let raw_items = value
            .as_array()
            .ok_or_else(|| definite("Clockify returned an invalid workspace list."))?;
        let truncated = raw_items.len() > 250;
        let items = raw_items
            .iter()
            .take(250)
            .map(|item| {
                let item = object(item, "workspace")?;
                Ok(serde_json::json!({
                    "id": required_string(item, &["id"], 300, "workspace ID")?,
                    "name": required_string(item, &["name"], 300, "workspace name")?,
                }))
            })
            .collect::<Result<Vec<_>, ClockifyError>>()?;
        Ok(self.account_result(serde_json::json!({ "items": items, "truncated": truncated })))
    }

    async fn list_projects(&self, params: ListProjectsParams) -> Result<serde_json::Value, ClockifyError> {
        let workspace_id = provider_id(&params.workspace_id).map_err(definite)?;
        let path = format!(
            "/workspaces/{}/projects?archived=false&page-size=500",
            encode_path(&workspace_id)
        );
        let value = self.request_json(ApiKind::Api, Method::GET, &path, None, false).await?;
        let raw_items = value
            .as_array()
            .ok_or_else(|| definite("Clockify returned an invalid project list."))?;
        let truncated = raw_items.len() >= 500;
        let items = raw_items
            .iter()
            .filter(|item| item.get("archived").and_then(serde_json::Value::as_bool) != Some(true))
            .take(500)
            .map(|item| {
                let item = object(item, "project")?;
                let client_name = item
                    .get("client")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|client| optional_string(client, &["name"], 300))
                    .or_else(|| optional_string(item, &["clientName"], 300));
                Ok(serde_json::json!({
                    "id": required_string(item, &["id"], 300, "project ID")?,
                    "name": required_string(item, &["name"], 300, "project name")?,
                    "client_name": client_name,
                }))
            })
            .collect::<Result<Vec<_>, ClockifyError>>()?;
        Ok(self.account_result(serde_json::json!({
            "workspace_id": workspace_id,
            "items": items,
            "truncated": truncated,
        })))
    }

    async fn get_time_entry(&self, params: GetTimeEntryParams) -> Result<TimeEntry, ClockifyError> {
        let workspace_id = provider_id(&params.workspace_id).map_err(definite)?;
        let entry_id = provider_id(&params.time_entry_id).map_err(definite)?;
        let path = format!(
            "/workspaces/{}/time-entries/{}",
            encode_path(&workspace_id),
            encode_path(&entry_id)
        );
        let value = self.request_json(ApiKind::Api, Method::GET, &path, None, false).await?;
        map_time_entry(&value, true)
    }

    async fn create_time_entry(&self, params: CreateTimeEntryParams) -> Result<serde_json::Value, ClockifyError> {
        let workspace_id = provider_id(&params.workspace_id).map_err(definite)?;
        let project_id = provider_id(&params.project_id).map_err(definite)?;
        validate_core(&params.entry)?;
        let path = format!(
            "/workspaces/{}/user/{}/time-entries",
            encode_path(&workspace_id),
            encode_path(&self.account_id)
        );
        let body = serde_json::json!({
            "start": params.entry.start,
            "end": params.entry.end,
            "description": params.entry.description,
            "billable": params.entry.billable,
            "projectId": project_id,
            "type": "REGULAR",
        });
        match self
            .request_json(ApiKind::Api, Method::POST, &path, Some(body), true)
            .await
        {
            Ok(value) => {
                let Ok(entry) = map_time_entry(&value, false) else {
                    return Ok(self.uncertain_result("create_time_entry"));
                };
                if ensure_confirmed(
                    &entry,
                    &workspace_id,
                    &project_id,
                    &self.account_id,
                    None,
                    &params.entry,
                )
                .is_err()
                {
                    return Ok(self.uncertain_result("create_time_entry"));
                }
                Ok(self.mutation_result("create_time_entry", "confirmed", Some(entry), None))
            }
            Err(ClockifyError::DeliveryUncertain) => Ok(self.uncertain_result("create_time_entry")),
            Err(error) => Err(error),
        }
    }

    async fn update_time_entry(&self, params: UpdateTimeEntryParams) -> Result<serde_json::Value, ClockifyError> {
        validate_expected_core(&params.expected)?;
        validate_core(&params.replacement)?;
        let workspace_id = provider_id(&params.workspace_id).map_err(definite)?;
        let project_id = provider_id(&params.project_id).map_err(definite)?;
        let entry_id = provider_id(&params.time_entry_id).map_err(definite)?;
        let mutation_lock = self.entry_mutation_lock(&workspace_id, &entry_id).await;
        let _mutation_guard = mutation_lock.lock().await;
        let _process_guard = self.acquire_process_mutation_lock(&workspace_id, &entry_id).await?;
        let current = self
            .get_time_entry(GetTimeEntryParams {
                workspace_id: workspace_id.clone(),
                time_entry_id: entry_id.clone(),
            })
            .await?;
        ensure_mutable_snapshot(
            &current,
            &workspace_id,
            &project_id,
            &self.account_id,
            &entry_id,
            &params.expected,
        )?;
        let path = format!(
            "/workspaces/{}/time-entries/{}",
            encode_path(&workspace_id),
            encode_path(&entry_id)
        );
        let mut body = serde_json::json!({
            "start": params.replacement.start,
            "end": params.replacement.end,
            "description": params.replacement.description,
            "billable": params.replacement.billable,
            "projectId": current.project_id,
            "tagIds": current.tag_ids,
            "customFields": current.custom_fields,
            "type": current.entry_type,
        });
        if let Some(task_id) = &current.task_id {
            body.as_object_mut()
                .expect("Clockify update payload is an object")
                .insert("taskId".into(), serde_json::Value::String(task_id.clone()));
        }
        match self
            .request_json(ApiKind::Api, Method::PUT, &path, Some(body), true)
            .await
        {
            Ok(value) => {
                let Ok(entry) = map_time_entry(&value, false) else {
                    return Ok(self.uncertain_result("update_time_entry"));
                };
                if ensure_confirmed(
                    &entry,
                    &workspace_id,
                    &project_id,
                    &self.account_id,
                    Some(&entry_id),
                    &params.replacement,
                )
                .is_err()
                {
                    return Ok(self.uncertain_result("update_time_entry"));
                }
                if !preservation_matches(&entry, &current) {
                    return Ok(self.uncertain_result("update_time_entry"));
                }
                Ok(self.mutation_result("update_time_entry", "confirmed", Some(entry), None))
            }
            Err(ClockifyError::DeliveryUncertain) => Ok(self.uncertain_result("update_time_entry")),
            Err(error) => Err(error),
        }
    }

    async fn delete_time_entry(&self, params: DeleteTimeEntryParams) -> Result<serde_json::Value, ClockifyError> {
        validate_expected_core(&params.expected)?;
        let workspace_id = provider_id(&params.workspace_id).map_err(definite)?;
        let project_id = provider_id(&params.project_id).map_err(definite)?;
        let entry_id = provider_id(&params.time_entry_id).map_err(definite)?;
        let mutation_lock = self.entry_mutation_lock(&workspace_id, &entry_id).await;
        let _mutation_guard = mutation_lock.lock().await;
        let _process_guard = self.acquire_process_mutation_lock(&workspace_id, &entry_id).await?;
        let current = self
            .get_time_entry(GetTimeEntryParams {
                workspace_id: workspace_id.clone(),
                time_entry_id: entry_id.clone(),
            })
            .await?;
        ensure_mutable_snapshot(
            &current,
            &workspace_id,
            &project_id,
            &self.account_id,
            &entry_id,
            &params.expected,
        )?;
        let path = format!(
            "/workspaces/{}/time-entries/{}",
            encode_path(&workspace_id),
            encode_path(&entry_id)
        );
        match self.request_empty(ApiKind::Api, Method::DELETE, &path, true).await {
            Ok(()) => Ok(self.mutation_result("delete_time_entry", "confirmed", Some(current), None)),
            Err(ClockifyError::DeliveryUncertain) => Ok(self.uncertain_result("delete_time_entry")),
            Err(error) => Err(error),
        }
    }

    async fn get_time_report(&self, params: GetTimeReportParams) -> Result<serde_json::Value, ClockifyError> {
        let workspace_id = provider_id(&params.workspace_id).map_err(definite)?;
        let project_id = provider_id(&params.project_id).map_err(definite)?;
        let start = parse_timestamp(&params.start, "report start")?;
        let end = parse_timestamp(&params.end, "report end")?;
        if end <= start || end.signed_duration_since(start) > chrono::Duration::days(366) {
            return Err(definite(
                "Clockify reports require an end after start and are limited to 366 days.",
            ));
        }
        if params.time_zone.is_empty() || params.time_zone.len() > 100 || params.time_zone.trim() != params.time_zone {
            return Err(definite(
                "Clockify report timezone must be a valid IANA timezone of at most 100 characters.",
            ));
        }
        let time_zone = params
            .time_zone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| definite("Clockify report timezone must be a valid IANA timezone."))?;
        let windows = report_windows(start, end);
        let mut entries = BTreeMap::<String, ReportEntry>::new();
        let mut covered = Vec::new();
        let mut missing = Vec::new();
        let mut requests = 0usize;
        let mut total_duration_seconds = 0u64;
        let mut reason: Option<&str> = None;
        let mut successful_pages = 0usize;
        let mut terminal_header_pages = 0usize;
        let mut continuation_header_pages = 0usize;
        let mut missing_or_unrecognized_header_pages = 0usize;
        let mut empty_page_completions = 0usize;
        let mut missing_total_windows = 0usize;
        let deadline = tokio::time::Instant::now() + REPORT_DEADLINE;

        for (index, (window_start, window_end)) in windows.iter().enumerate() {
            let mut page = 1usize;
            let mut window_total = None;
            let mut complete = false;
            loop {
                if requests >= MAX_REPORT_REQUESTS {
                    reason = Some("request_budget");
                    break;
                }
                if entries.len() >= MAX_FETCHED_ENTRIES {
                    reason = Some("entry_limit");
                    break;
                }
                let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
                    if entries.is_empty() && covered.is_empty() {
                        return Err(definite(
                            "Clockify report deadline was reached before any data was returned.",
                        ));
                    }
                    reason = Some("deadline");
                    break;
                };
                requests += 1;
                let (provider_start, provider_end) = provider_report_range(*window_start, *window_end)?;
                let body = serde_json::json!({
                    "dateRangeStart": provider_start,
                    "dateRangeEnd": provider_end,
                    "dateRangeType": "ABSOLUTE",
                    "timeZone": params.time_zone,
                    "amountShown": "HIDE_AMOUNT",
                    "users": { "ids": [self.account_id], "contains": "CONTAINS" },
                    "projects": { "ids": [project_id], "contains": "CONTAINS" },
                    "detailedFilter": {
                        "page": page,
                        "pageSize": REPORT_PAGE_SIZE,
                        "sortColumn": "DATE"
                    },
                    "sortOrder": "ASCENDING",
                    "exportType": "JSON",
                });
                let path = format!("/workspaces/{}/reports/detailed", encode_path(&workspace_id));
                let response = match tokio::time::timeout(
                    remaining,
                    self.request_json_with_headers(ApiKind::Reports, Method::POST, &path, Some(body), false),
                )
                .await
                {
                    Err(_) if entries.is_empty() && covered.is_empty() => {
                        return Err(definite(
                            "Clockify report deadline was reached before any data was returned.",
                        ));
                    }
                    Err(_) => {
                        reason = Some("deadline");
                        break;
                    }
                    Ok(result) => match result {
                        Ok(response) => response,
                        Err(ClockifyError::Definite(message))
                            if message.contains("authentication is no longer valid") || message.contains("denied") =>
                        {
                            return Err(ClockifyError::Definite(message));
                        }
                        Err(ClockifyError::Definite(message)) if message.contains("rate limit") => {
                            reason = Some("rate_limited");
                            break;
                        }
                        Err(ClockifyError::Definite(message)) if entries.is_empty() && covered.is_empty() => {
                            return Err(ClockifyError::Definite(message));
                        }
                        Err(ClockifyError::Definite(_)) | Err(ClockifyError::DeliveryUncertain) => {
                            reason = Some("provider_limit");
                            break;
                        }
                    },
                };
                let (page_total, page_entries) = match parse_report_page(&response.0, page == 1) {
                    Ok(parsed) => parsed,
                    Err(error) if entries.is_empty() && covered.is_empty() => return Err(error),
                    Err(_) => {
                        reason = Some("provider_limit");
                        break;
                    }
                };
                successful_pages += 1;
                let page_is_empty = page_entries.is_empty();
                if page == 1 {
                    window_total = page_total;
                }
                for entry in page_entries {
                    if entries.get(&entry.id).is_some_and(|previous| previous != &entry) {
                        reason = Some("provider_changed");
                        break;
                    }
                    entries.insert(entry.id.clone(), entry);
                    if entries.len() >= MAX_FETCHED_ENTRIES {
                        reason = Some("entry_limit");
                        break;
                    }
                }
                if reason.is_some() {
                    break;
                }
                match last_page_evidence(&response.1) {
                    LastPageEvidence::Terminal => {
                        terminal_header_pages += 1;
                        if page == 1 && page_is_empty && window_total.is_some_and(|total| total != 0) {
                            reason = Some("provider_changed");
                            break;
                        }
                        if page == 1 && page_is_empty && window_total.is_none() {
                            window_total = Some(0);
                        }
                        complete = true;
                        break;
                    }
                    LastPageEvidence::More => {
                        continuation_header_pages += 1;
                        page += 1;
                    }
                    LastPageEvidence::MissingOrUnrecognized => {
                        missing_or_unrecognized_header_pages += 1;
                        if page_is_empty && (page > 1 || window_total.unwrap_or_default() == 0) {
                            if page == 1 && window_total.is_none() {
                                window_total = Some(0);
                            }
                            empty_page_completions += 1;
                            complete = true;
                            break;
                        }
                        reason = Some("pagination_evidence_missing");
                        break;
                    }
                }
            }
            if complete && window_total.is_some() {
                total_duration_seconds = total_duration_seconds.saturating_add(window_total.unwrap_or_default());
                covered.push(interval_json(*window_start, *window_end));
            } else {
                if complete {
                    missing_total_windows += 1;
                    reason.get_or_insert("provider_total_missing");
                } else {
                    reason.get_or_insert("provider_limit");
                }
                missing.push(interval_json(*window_start, *window_end));
                missing.extend(
                    windows
                        .iter()
                        .skip(index + 1)
                        .map(|(start, end)| interval_json(*start, *end)),
                );
                break;
            }
        }

        let mut entries = entries.into_values().collect::<Vec<_>>();
        if !params.include_running && entries.iter().any(|entry| entry.running) {
            entries.retain(|entry| !entry.running);
            reason.get_or_insert("provider_limit");
            covered.clear();
            missing = windows.iter().map(|(start, end)| interval_json(*start, *end)).collect();
        }
        let completeness = if missing.is_empty() && reason.is_none() {
            "complete"
        } else if entries.is_empty() {
            "unavailable"
        } else {
            "partial"
        };
        let fetched_duration_seconds = entries
            .iter()
            .fold(0u64, |total, entry| total.saturating_add(entry.duration_seconds));
        let mut groups = group_entries(&entries, &params.group_by, time_zone);
        let groups_truncated = groups.len() > MAX_EXPOSED_GROUPS;
        groups.truncate(MAX_EXPOSED_GROUPS);
        let detail_limit = match params.detail {
            ReportDetail::None => 0,
            ReportDetail::Summary => 50,
            ReportDetail::Full => MAX_EXPOSED_ENTRIES,
        };
        let details_truncated = !matches!(params.detail, ReportDetail::None) && entries.len() > detail_limit;
        entries.truncate(detail_limit);
        let mut result = self.account_result(serde_json::json!({
            "period": { "start": params.start, "end": params.end, "time_zone": params.time_zone },
            "scope": { "workspace_id": workspace_id, "project_id": project_id, "user_id": self.account_id },
            "metric": "clockify-report total",
            "completeness": completeness,
            "reason": reason,
            "total_duration_seconds": if completeness == "complete" { Some(total_duration_seconds) } else { None },
            "fetched_duration_seconds": if completeness == "partial" { Some(fetched_duration_seconds) } else { None },
            "requests": requests,
            "windows": windows.len(),
            "provider_pagination": {
                "header": "last-page",
                "successful_pages": successful_pages,
                "terminal_header_pages": terminal_header_pages,
                "continuation_header_pages": continuation_header_pages,
                "missing_or_unrecognized_header_pages": missing_or_unrecognized_header_pages,
                "empty_page_completions": empty_page_completions,
                "missing_total_windows": missing_total_windows,
            },
            "covered_intervals": covered,
            "missing_intervals": missing,
            "groups": groups,
            "groups_truncated": groups_truncated,
            "entries": entries,
            "details_truncated": details_truncated,
            "provider_may_have_changed": true,
        }));
        bound_report_output(&mut result);
        Ok(result)
    }

    fn account_result(&self, value: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "provider": "clockify",
            "connection_id": self.connection_id,
            "realm": self.realm,
            "account": {
                "id": self.account_id,
                "name": self.account_name,
                "email": self.account_email,
            },
            "result": value,
        })
    }

    fn mutation_result(
        &self,
        operation: &str,
        delivery: &str,
        entry: Option<TimeEntry>,
        guidance: Option<&str>,
    ) -> serde_json::Value {
        self.account_result(serde_json::json!({
            "operation": operation,
            "delivery": delivery,
            "entry": entry,
            "guidance": guidance,
        }))
    }

    fn uncertain_result(&self, operation: &str) -> serde_json::Value {
        self.mutation_result(
            operation,
            "delivery_uncertain",
            None,
            Some("Inspect Clockify before requesting another mutation; do not retry automatically."),
        )
    }

    async fn entry_mutation_lock(&self, workspace_id: &str, entry_id: &str) -> Arc<Mutex<()>> {
        let key = (workspace_id.to_owned(), entry_id.to_owned());
        let mut locks = self.mutation_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    async fn acquire_process_mutation_lock(&self, workspace_id: &str, entry_id: &str) -> Result<File, ClockifyError> {
        let digest = Sha256::digest(format!("{workspace_id}\0{entry_id}").as_bytes());
        let path = self.mutation_lock_dir.join(format!("{}.lock", hex::encode(digest)));
        tokio::task::spawn_blocking(move || open_and_lock(&path))
            .await
            .map_err(|_| definite("Clockify mutation lock task failed."))?
    }

    async fn request_json(
        &self,
        kind: ApiKind,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        mutation: bool,
    ) -> Result<serde_json::Value, ClockifyError> {
        Ok(self
            .request_json_with_headers(kind, method, path, body, mutation)
            .await?
            .0)
    }

    async fn request_empty(
        &self,
        kind: ApiKind,
        method: Method,
        path: &str,
        mutation: bool,
    ) -> Result<(), ClockifyError> {
        let response = self.send(kind, method, path, None, mutation).await?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PROVIDER_BYTES as u64)
        {
            return Err(if mutation {
                ClockifyError::DeliveryUncertain
            } else {
                definite("Clockify response exceeded the safe size limit.")
            });
        }
        Ok(())
    }

    async fn request_json_with_headers(
        &self,
        kind: ApiKind,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        mutation: bool,
    ) -> Result<(serde_json::Value, reqwest::header::HeaderMap), ClockifyError> {
        let response = self.send(kind, method, path, body, mutation).await?;
        let headers = response.headers().clone();
        let bytes = read_bounded_response(response, MAX_PROVIDER_BYTES, mutation).await?;
        let value = serde_json::from_slice(&bytes).map_err(|_| {
            if mutation {
                ClockifyError::DeliveryUncertain
            } else {
                definite("Clockify returned invalid JSON.")
            }
        })?;
        Ok((value, headers))
    }

    async fn send(
        &self,
        kind: ApiKind,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        mutation: bool,
    ) -> Result<reqwest::Response, ClockifyError> {
        let base = match kind {
            ApiKind::Api => &self.api_base,
            ApiKind::Reports => &self.reports_base,
        };
        let mut request = self
            .http
            .request(method, format!("{base}{path}"))
            .header("X-Api-Key", &self.api_key)
            .header("Accept", "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| {
            if mutation {
                ClockifyError::DeliveryUncertain
            } else {
                definite("Clockify request failed.")
            }
        })?;
        match response.status() {
            StatusCode::UNAUTHORIZED => Err(definite(
                "Clockify authentication is no longer valid. Reconnect this account in Settings → Plugins.",
            )),
            StatusCode::FORBIDDEN => Err(definite(
                "Clockify denied this request under the current plan or policy.",
            )),
            StatusCode::TOO_MANY_REQUESTS => Err(definite("Clockify rate limit was reached.")),
            status if mutation && status.is_server_error() => Err(ClockifyError::DeliveryUncertain),
            status if matches!(status, StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY) && !mutation => {
                Err(sanitized_provider_failure(response, status).await)
            }
            status if !status.is_success() => Err(definite(format!("Clockify request failed with status {status}."))),
            _ => Ok(response),
        }
    }
}

#[derive(Clone, Copy)]
enum ApiKind {
    Api,
    Reports,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastPageEvidence {
    Terminal,
    More,
    MissingOrUnrecognized,
}

fn last_page_evidence(headers: &reqwest::header::HeaderMap) -> LastPageEvidence {
    let Some(value) = headers.get("last-page").and_then(|value| value.to_str().ok()) else {
        return LastPageEvidence::MissingOrUnrecognized;
    };
    let value = value.trim();
    if value == "1" || value.eq_ignore_ascii_case("true") {
        LastPageEvidence::Terminal
    } else if value == "0" || value.eq_ignore_ascii_case("false") {
        LastPageEvidence::More
    } else {
        LastPageEvidence::MissingOrUnrecognized
    }
}

async fn read_bounded_response(
    mut response: reqwest::Response,
    limit: usize,
    mutation: bool,
) -> Result<Vec<u8>, ClockifyError> {
    let failure = || {
        if mutation {
            ClockifyError::DeliveryUncertain
        } else {
            definite("Clockify response exceeded the safe size limit.")
        }
    };
    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(failure());
    }
    let mut bytes = Vec::with_capacity(response.content_length().unwrap_or_default().min(limit as u64) as usize);
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        if mutation {
            ClockifyError::DeliveryUncertain
        } else {
            definite("Clockify response could not be read.")
        }
    })? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(failure());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn sanitized_provider_failure(mut response: reqwest::Response, status: StatusCode) -> ClockifyError {
    let generic = || definite(format!("Clockify request failed with status {status}."));
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_ERROR_BYTES as u64)
    {
        return generic();
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_PROVIDER_ERROR_BYTES as u64) as usize,
    );
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => return generic(),
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_PROVIDER_ERROR_BYTES {
            return generic();
        }
        bytes.extend_from_slice(&chunk);
    }
    let Some(diagnostic) = sanitized_provider_diagnostic(&bytes) else {
        return generic();
    };
    definite(format!("Clockify request failed with status {status} ({diagnostic})."))
}

fn sanitized_provider_diagnostic(bytes: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    let value = value.as_object()?;
    let message = value.get("message")?.as_str()?;
    let lower = message.to_ascii_lowercase();
    let field = [
        ("daterangestart", "date_range_start"),
        ("daterangeend", "date_range_end"),
        ("detailedfilter", "detailed_filter"),
        ("pagesize", "page_size"),
        ("sortcolumn", "sort_column"),
        ("amountshown", "amount_shown"),
        ("timezone", "time_zone"),
        ("projects", "projects"),
        ("users", "users"),
    ]
    .into_iter()
    .find_map(|(needle, field)| lower.contains(needle).then_some(field));
    let reason = if lower.contains("required") || lower.contains("missing") || lower.contains("must not be null") {
        "required_field"
    } else if lower.contains("unsupported")
        || lower.contains("not supported")
        || lower.contains("not allowed")
        || lower.contains("enum")
    {
        "unsupported_value"
    } else if lower.contains("invalid")
        || lower.contains("format")
        || lower.contains("deserialize")
        || lower.contains("parse")
    {
        "invalid_value"
    } else {
        "provider_validation"
    };
    let code = value.get("code").and_then(serde_json::Value::as_i64);
    let mut diagnostic = format!("provider_reason={reason}");
    if let Some(field) = field {
        diagnostic.push_str(", field=");
        diagnostic.push_str(field);
    }
    if let Some(code) = code {
        diagnostic.push_str(", provider_code=");
        diagnostic.push_str(&code.to_string());
    }
    Some(diagnostic)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct ReportEntry {
    id: String,
    description: String,
    task_name: Option<String>,
    start: String,
    end: Option<String>,
    duration_seconds: u64,
    running: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct TimeEntry {
    id: String,
    workspace_id: String,
    project_id: String,
    user_id: String,
    start: String,
    end: String,
    description: String,
    billable: bool,
    is_locked: bool,
    task_id: Option<String>,
    tag_ids: Vec<String>,
    custom_fields: Vec<TimeEntryCustomField>,
    #[serde(rename = "type")]
    entry_type: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct TimeEntryCustomField {
    custom_field_id: String,
    #[serde(alias = "type")]
    source_type: String,
    value: serde_json::Value,
}

fn map_time_entry(value: &serde_json::Value, require_preservation: bool) -> Result<TimeEntry, ClockifyError> {
    let item = object(value, "time entry")?;
    let interval = item
        .get("timeInterval")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| definite("Clockify time entry interval is missing."))?;
    let start = required_string(interval, &["start"], 100, "time entry start")?;
    let end = required_string(interval, &["end"], 100, "time entry end")?;
    let start_time = parse_timestamp(&start, "time entry start")?;
    let end_time = parse_timestamp(&end, "time entry end")?;
    if end_time <= start_time {
        return Err(definite("Clockify time entry interval is invalid."));
    }
    let raw_tags = item.get("tagIds").and_then(serde_json::Value::as_array);
    let raw_fields = item.get("customFieldValues").and_then(serde_json::Value::as_array);
    if require_preservation && (raw_tags.is_none() || raw_fields.is_none()) {
        return Err(definite("Clockify time entry preservation metadata is missing."));
    }
    let tag_ids = raw_tags
        .into_iter()
        .flatten()
        .take(51)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| definite("Clockify time entry tags are invalid."))
                .and_then(|value| provider_id(value).map_err(definite))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let custom_fields = raw_fields
        .into_iter()
        .flatten()
        .take(51)
        .map(|value| {
            let mut field = serde_json::from_value::<TimeEntryCustomField>(value.clone())
                .map_err(|_| definite("Clockify time entry custom fields are invalid."))?;
            field.custom_field_id = provider_id(&field.custom_field_id).map_err(definite)?;
            field.source_type = bounded_text(&field.source_type, 100, "custom field source type").map_err(definite)?;
            if !is_bounded_json(&field.value) {
                return Err(definite("Clockify time entry custom fields are invalid."));
            }
            Ok(field)
        })
        .collect::<Result<Vec<_>, ClockifyError>>()?;
    if tag_ids.len() > 50 || custom_fields.len() > 50 {
        return Err(definite("Clockify time entry has too much preservation metadata."));
    }
    if !matches!(
        serde_json::to_vec(&custom_fields),
        Ok(bytes) if bytes.len() <= MAX_CUSTOM_FIELDS_BYTES
    ) {
        return Err(definite(
            "Clockify time entry custom fields exceed the safe size limit.",
        ));
    }
    let entry_type = required_string(item, &["type"], 20, "time entry type")?;
    if entry_type != "REGULAR" && entry_type != "BREAK" {
        return Err(definite("Clockify time entry type is invalid."));
    }
    Ok(TimeEntry {
        id: required_string(item, &["id"], 300, "time entry ID")?,
        workspace_id: required_string(item, &["workspaceId"], 300, "workspace ID")?,
        project_id: required_string(item, &["projectId"], 300, "project ID")?,
        user_id: required_string(item, &["userId"], 300, "user ID")?,
        start,
        end,
        description: raw_optional_string(item, &["description"], 3_000, "time entry description")?.unwrap_or_default(),
        billable: item
            .get("billable")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| definite("Clockify time entry billable state is invalid."))?,
        is_locked: item
            .get("isLocked")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| definite("Clockify time entry lock state is invalid."))?,
        task_id: item
            .get("taskId")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(provider_id)
            .transpose()
            .map_err(definite)?,
        tag_ids,
        custom_fields,
        entry_type,
    })
}

fn map_report_entry(value: &serde_json::Value) -> Result<ReportEntry, ClockifyError> {
    let item = object(value, "report entry")?;
    let interval = item
        .get("timeInterval")
        .and_then(serde_json::Value::as_object)
        .unwrap_or(item);
    let start = required_string(interval, &["start"], 100, "report entry start")?;
    parse_timestamp(&start, "report entry start")?;
    let end = optional_string(interval, &["end"], 100);
    if let Some(end) = &end
        && parse_timestamp(end, "report entry end")? < parse_timestamp(&start, "report entry start")?
    {
        return Err(definite("Clockify report entry interval is invalid."));
    }
    Ok(ReportEntry {
        id: required_string(item, &["_id", "id", "timeEntryId"], 300, "report entry ID")?,
        description: optional_string(item, &["description"], 2_000).unwrap_or_default(),
        task_name: optional_string(item, &["taskName"], 300).or_else(|| {
            item.get("task")
                .and_then(serde_json::Value::as_object)
                .and_then(|task| optional_string(task, &["name"], 300))
        }),
        start,
        end: end.clone(),
        duration_seconds: duration_seconds(
            interval
                .get("duration")
                .or_else(|| item.get("duration"))
                .ok_or_else(|| definite("Clockify report entry duration is missing."))?,
        )?,
        running: end.is_none(),
    })
}

fn ensure_mutable_snapshot(
    current: &TimeEntry,
    workspace_id: &str,
    project_id: &str,
    user_id: &str,
    entry_id: &str,
    expected: &ExpectedTimeEntryCore,
) -> Result<(), ClockifyError> {
    if current.workspace_id != workspace_id
        || current.project_id != project_id
        || current.user_id != user_id
        || current.id != entry_id
    {
        return Err(definite(
            "Clockify time entry is outside the selected account, workspace, or project.",
        ));
    }
    if current.is_locked {
        return Err(definite("Clockify time entry is locked and cannot be changed."));
    }
    if !expected_core_matches(current, expected) {
        return Err(definite(
            "Clockify time entry changed after the expected snapshot; inspect it again before mutating.",
        ));
    }
    Ok(())
}

fn ensure_confirmed(
    entry: &TimeEntry,
    workspace_id: &str,
    project_id: &str,
    user_id: &str,
    entry_id: Option<&str>,
    expected: &TimeEntryCore,
) -> Result<(), ClockifyError> {
    if entry.workspace_id != workspace_id
        || entry.project_id != project_id
        || entry.user_id != user_id
        || entry_id.is_some_and(|id| entry.id != id)
        || !replacement_core_matches(entry, expected)
    {
        return Err(ClockifyError::DeliveryUncertain);
    }
    Ok(())
}

fn replacement_core_matches(entry: &TimeEntry, expected: &TimeEntryCore) -> bool {
    parse_timestamp(&entry.start, "start").ok() == parse_timestamp(&expected.start, "start").ok()
        && parse_timestamp(&entry.end, "end").ok() == parse_timestamp(&expected.end, "end").ok()
        && entry.description == expected.description
        && entry.billable == expected.billable
}

fn expected_core_matches(entry: &TimeEntry, expected: &ExpectedTimeEntryCore) -> bool {
    parse_timestamp(&entry.start, "start").ok() == parse_timestamp(&expected.start, "start").ok()
        && parse_timestamp(&entry.end, "end").ok() == parse_timestamp(&expected.end, "end").ok()
        && entry.description == expected.description
        && entry.billable == expected.billable
}

fn preservation_matches(entry: &TimeEntry, expected: &TimeEntry) -> bool {
    let mut entry_tags = entry.tag_ids.clone();
    let mut expected_tags = expected.tag_ids.clone();
    entry_tags.sort();
    expected_tags.sort();
    let mut entry_fields = entry.custom_fields.clone();
    let mut expected_fields = expected.custom_fields.clone();
    let field_order = |left: &TimeEntryCustomField, right: &TimeEntryCustomField| {
        left.custom_field_id
            .cmp(&right.custom_field_id)
            .then_with(|| left.source_type.cmp(&right.source_type))
    };
    entry_fields.sort_by(field_order);
    expected_fields.sort_by(field_order);
    entry.task_id == expected.task_id
        && entry.entry_type == expected.entry_type
        && entry_tags == expected_tags
        && entry_fields == expected_fields
}

fn validate_core(core: &TimeEntryCore) -> Result<(), ClockifyError> {
    let start = parse_timestamp(&core.start, "time entry start")?;
    let end = parse_timestamp(&core.end, "time entry end")?;
    if end <= start {
        return Err(definite("Clockify time entry end must be after start."));
    }
    if core.description.trim().is_empty() || core.description.len() > 3_000 {
        return Err(definite(
            "Clockify time entry description must contain 1 to 3,000 characters.",
        ));
    }
    Ok(())
}

fn validate_expected_core(core: &ExpectedTimeEntryCore) -> Result<(), ClockifyError> {
    let start = parse_timestamp(&core.start, "expected time entry start")?;
    let end = parse_timestamp(&core.end, "expected time entry end")?;
    if end <= start {
        return Err(definite("Expected Clockify time entry end must be after start."));
    }
    if core.description.len() > 3_000 {
        return Err(definite(
            "Expected Clockify time entry description must contain at most 3,000 characters.",
        ));
    }
    Ok(())
}

fn group_entries(entries: &[ReportEntry], by: &GroupBy, time_zone: chrono_tz::Tz) -> Vec<serde_json::Value> {
    let mut groups = HashMap::<String, u64>::new();
    for entry in entries {
        let key = match by {
            GroupBy::Task => entry.task_name.clone().unwrap_or_else(|| "(no task)".into()),
            GroupBy::Description => {
                if entry.description.is_empty() {
                    "(no description)".into()
                } else {
                    entry.description.clone()
                }
            }
            GroupBy::Day => parse_timestamp(&entry.start, "entry start")
                .map(|value| value.with_timezone(&time_zone))
                .map(|value| format!("{:04}-{:02}-{:02}", value.year(), value.month(), value.day()))
                .unwrap_or_else(|_| "invalid".into()),
        };
        let total = groups.entry(key).or_default();
        *total = total.saturating_add(entry.duration_seconds);
    }
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    groups
        .into_iter()
        .map(|(key, duration_seconds)| serde_json::json!({ "key": key, "duration_seconds": duration_seconds }))
        .collect()
}

fn report_windows(start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut windows = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let next = (cursor + chrono::Duration::days(31)).min(end);
        windows.push((cursor, next));
        cursor = next;
    }
    windows
}

fn provider_report_range(
    start: DateTime<Utc>,
    exclusive_end: DateTime<Utc>,
) -> Result<(String, String), ClockifyError> {
    if !start.timestamp_subsec_nanos().is_multiple_of(1_000)
        || !exclusive_end.timestamp_subsec_nanos().is_multiple_of(1_000)
    {
        return Err(definite(
            "Clockify report interval boundaries support at most microsecond precision.",
        ));
    }
    let inclusive_end = exclusive_end
        .checked_sub_signed(chrono::Duration::microseconds(1))
        .filter(|inclusive_end| inclusive_end >= &start)
        .ok_or_else(|| definite("Clockify report intervals must be at least one microsecond."))?;
    Ok((
        start.to_rfc3339_opts(SecondsFormat::Micros, true),
        inclusive_end.to_rfc3339_opts(SecondsFormat::Micros, true),
    ))
}

fn report_total(value: &serde_json::Map<String, serde_json::Value>) -> Result<Option<u64>, ClockifyError> {
    let Some(total) = value
        .get("totals")
        .and_then(serde_json::Value::as_array)
        .and_then(|totals| totals.first())
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(None);
    };
    total
        .get("totalTime")
        .or_else(|| total.get("duration"))
        .map(duration_seconds)
        .transpose()
}

fn parse_report_page(
    value: &serde_json::Value,
    include_total: bool,
) -> Result<(Option<u64>, Vec<ReportEntry>), ClockifyError> {
    let page = object(value, "report")?;
    let total = if include_total { report_total(page)? } else { None };
    let entries = page
        .get("timeentries")
        .or_else(|| page.get("timeEntries"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| definite("Clockify returned an invalid report page."))?
        .iter()
        .map(map_report_entry)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((total, entries))
}

fn duration_seconds(value: &serde_json::Value) -> Result<u64, ClockifyError> {
    if let Some(value) = value.as_u64() {
        return safe_seconds(value as f64).ok_or_else(|| definite("Clockify duration is invalid."));
    }
    let text = value
        .as_str()
        .ok_or_else(|| definite("Clockify duration is invalid."))?;
    if is_decimal(text) {
        return text
            .parse::<f64>()
            .ok()
            .and_then(safe_seconds)
            .ok_or_else(|| definite("Clockify duration is invalid."));
    }
    parse_iso_duration(text).ok_or_else(|| definite("Clockify duration is invalid."))
}

fn parse_iso_duration(value: &str) -> Option<u64> {
    let value = value.strip_prefix('P')?;
    let (days_text, time) = value.split_once('T')?;
    if time.contains('T') {
        return None;
    }
    let (days, mut found_component) = if days_text.is_empty() {
        (0.0, false)
    } else {
        let days = days_text.strip_suffix('D')?;
        if days.is_empty() || !days.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        (days.parse::<f64>().ok()?, true)
    };
    let mut rest = time;
    let mut total = days * 86_400.0;
    for (suffix, factor, decimal) in [('H', 3_600.0, false), ('M', 60.0, false), ('S', 1.0, true)] {
        if let Some(index) = rest.find(suffix) {
            let text = &rest[..index];
            if text.is_empty()
                || if decimal {
                    !is_decimal(text)
                } else {
                    !text.bytes().all(|byte| byte.is_ascii_digit())
                }
            {
                return None;
            }
            let number = text.parse::<f64>().ok()?;
            total += number * factor;
            rest = &rest[index + 1..];
            found_component = true;
        }
    }
    if !rest.is_empty() || !found_component {
        return None;
    }
    safe_seconds(total)
}

fn is_decimal(value: &str) -> bool {
    let mut parts = value.split('.');
    let Some(integer) = parts.next() else {
        return false;
    };
    if integer.is_empty() || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(fraction) => {
            !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit()) && parts.next().is_none()
        }
    }
}

fn safe_seconds(value: f64) -> Option<u64> {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    let rounded = value.round();
    (rounded.is_finite() && (0.0..=MAX_SAFE_INTEGER).contains(&rounded)).then_some(rounded as u64)
}

fn bound_report_output(value: &mut serde_json::Value) {
    while serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() > MAX_OUTPUT_BYTES) {
        let Some(result) = value.get_mut("result").and_then(serde_json::Value::as_object_mut) else {
            break;
        };
        if let Some(entries) = result.get_mut("entries").and_then(serde_json::Value::as_array_mut)
            && !entries.is_empty()
        {
            entries.truncate(entries.len() / 2);
            result.insert("details_truncated".into(), serde_json::Value::Bool(true));
            continue;
        }
        if let Some(groups) = result.get_mut("groups").and_then(serde_json::Value::as_array_mut)
            && !groups.is_empty()
        {
            groups.truncate(groups.len() / 2);
            result.insert("groups_truncated".into(), serde_json::Value::Bool(true));
            continue;
        }
        result.insert("completeness".into(), serde_json::Value::String("unavailable".into()));
        result.insert("reason".into(), serde_json::Value::String("output_limit".into()));
        break;
    }
}

fn interval_json(start: DateTime<Utc>, end: DateTime<Utc>) -> serde_json::Value {
    serde_json::json!({ "start": start.to_rfc3339(), "end": end.to_rfc3339() })
}

fn parse_timestamp(value: &str, label: &str) -> Result<DateTime<Utc>, ClockifyError> {
    if value.len() > 100 || value.trim() != value {
        return Err(definite(format!(
            "Clockify {label} must be an RFC 3339 timestamp with an explicit offset."
        )));
    }
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| {
            definite(format!(
                "Clockify {label} must be an RFC 3339 timestamp with an explicit offset."
            ))
        })
}

fn provider_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 300 || value.chars().any(|character| matches!(character, '/' | '?' | '#')) {
        return Err("Clockify provider identifier is invalid.".into());
    }
    Ok(value.into())
}

fn encode_path(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn object<'a>(
    value: &'a serde_json::Value,
    label: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>, ClockifyError> {
    value
        .as_object()
        .ok_or_else(|| definite(format!("Clockify returned an invalid {label}.")))
}

fn required_string(
    value: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
    max_len: usize,
    label: &str,
) -> Result<String, ClockifyError> {
    optional_string(value, keys, max_len).ok_or_else(|| definite(format!("Clockify {label} is missing.")))
}

fn optional_string(
    value: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
    max_len: usize,
) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= max_len)
        .map(str::to_owned)
}

fn raw_optional_string(
    value: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
    max_len: usize,
    label: &str,
) -> Result<Option<String>, ClockifyError> {
    for key in keys {
        let Some(value) = value.get(*key) else {
            continue;
        };
        if value.is_null() {
            return Ok(None);
        }
        let text = value
            .as_str()
            .ok_or_else(|| definite(format!("Clockify {label} is invalid.")))?;
        if text.len() > max_len {
            return Err(definite(format!("Clockify {label} exceeds the safe size limit.")));
        }
        return Ok(Some(text.to_owned()));
    }
    Ok(None)
}

fn is_bounded_json(value: &serde_json::Value) -> bool {
    fn visit(value: &serde_json::Value, depth: usize) -> bool {
        if depth > 10 {
            return false;
        }
        match value {
            serde_json::Value::Null | serde_json::Value::Bool(_) => true,
            serde_json::Value::String(value) => value.len() <= MAX_CUSTOM_FIELD_STRING_BYTES,
            serde_json::Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
            serde_json::Value::Array(values) => {
                values.len() <= 100 && values.iter().all(|value| visit(value, depth + 1))
            }
            serde_json::Value::Object(values) => {
                values.len() <= 100
                    && values
                        .iter()
                        .all(|(key, value)| key.len() <= 300 && visit(value, depth + 1))
            }
        }
    }
    visit(value, 0)
}

fn tool_from_result(result: Result<serde_json::Value, ClockifyError>) -> CallToolResult {
    match result {
        Ok(value) => {
            let text = serde_json::to_string(&value).unwrap_or_else(|_| "Clockify result unavailable".into());
            let mut result = CallToolResult::success(vec![Content::text(text)]);
            result.structured_content = Some(value);
            result
        }
        Err(ClockifyError::Definite(message)) => CallToolResult::error(vec![Content::text(message)]),
        Err(ClockifyError::DeliveryUncertain) => {
            let value = serde_json::json!({
                "provider": "clockify",
                "delivery": "delivery_uncertain",
                "guidance": "Inspect Clockify before requesting another mutation; do not retry automatically.",
            });
            let mut result = CallToolResult::success(vec![Content::text(value.to_string())]);
            result.structured_content = Some(value);
            result
        }
    }
}

fn definite(message: impl Into<String>) -> ClockifyError {
    ClockifyError::Definite(message.into())
}

fn required_env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing {name}"))
}

fn private_directory_env(name: &str) -> Result<PathBuf, String> {
    let value = required_env(name)?;
    if value.len() > 4_096 {
        return Err(format!("invalid {name}"));
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!("invalid {name}"));
    }
    Ok(path)
}

fn open_and_lock(path: &Path) -> Result<File, ClockifyError> {
    let parent = path
        .parent()
        .ok_or_else(|| definite("Clockify mutation lock path is invalid."))?;
    std::fs::create_dir_all(parent).map_err(|_| definite("Clockify mutation lock directory is unavailable."))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| definite("Clockify mutation lock directory could not be secured."))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| definite("Clockify mutation lock file is unavailable."))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| definite("Clockify mutation lock file could not be secured."))?;
    }
    file.lock_exclusive()
        .map_err(|_| definite("Clockify mutation lock could not be acquired."))?;
    Ok(file)
}

fn bounded_text(value: &str, max_len: usize, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > max_len {
        return Err(format!("invalid {label}"));
    }
    Ok(value.into())
}

fn parse_realm(value: &str) -> Result<ClockifyRealm, String> {
    match value {
        "global" => Ok(ClockifyRealm::Global),
        "eu" => Ok(ClockifyRealm::Eu),
        "us" => Ok(ClockifyRealm::Us),
        "uk" => Ok(ClockifyRealm::Uk),
        "au" => Ok(ClockifyRealm::Au),
        _ => Err("unsupported Clockify data region".into()),
    }
}

fn reports_base(realm: ClockifyRealm) -> &'static str {
    match realm {
        ClockifyRealm::Global => "https://reports.api.clockify.me/v1",
        ClockifyRealm::Eu => "https://euc1.clockify.me/report/v1",
        ClockifyRealm::Us => "https://use2.clockify.me/report/v1",
        ClockifyRealm::Uk => "https://euw2.clockify.me/report/v1",
        ClockifyRealm::Au => "https://apse2.clockify.me/report/v1",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fixture_client(server: &MockServer) -> ClockifyClient {
        ClockifyClient {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            api_base: server.uri(),
            reports_base: server.uri(),
            api_key: "fixture-api-key".into(),
            account_id: "account-1".into(),
            account_name: "Fixture Account".into(),
            account_email: Some("fixture@example.test".into()),
            connection_id: "connection-1".into(),
            realm: "global".into(),
            mutation_lock_dir: std::env::temp_dir()
                .join(format!("ellmsha-clockify-test-locks-{}", server.address().port())),
            mutation_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn time_entry(project_id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "entry-1",
            "workspaceId": "workspace-1",
            "projectId": project_id,
            "userId": "account-1",
            "description": "Fixture work",
            "billable": true,
            "isLocked": false,
            "taskId": "task-1",
            "tagIds": ["tag-b", "tag-a"],
            "customFieldValues": [{
                "customFieldId": "field-1",
                "sourceType": "PROJECT",
                "value": "fixture"
            }],
            "type": "REGULAR",
            "timeInterval": {
                "start": "2026-10-01T10:00:00Z",
                "end": "2026-10-01T11:00:00Z"
            }
        })
    }

    fn fixture_core() -> TimeEntryCore {
        TimeEntryCore {
            start: "2026-10-01T10:00:00Z".into(),
            end: "2026-10-01T11:00:00Z".into(),
            description: "Fixture work".into(),
            billable: true,
        }
    }

    fn fixture_expected() -> ExpectedTimeEntryCore {
        ExpectedTimeEntryCore {
            start: "2026-10-01T10:00:00Z".into(),
            end: "2026-10-01T11:00:00Z".into(),
            description: "Fixture work".into(),
            billable: true,
        }
    }

    #[test]
    fn clockify_tool_annotations_distinguish_reads_and_mutations() {
        let tools = ClockifyStdioServer::tool_router().list_all();
        assert_eq!(tools.len(), 7);
        assert_eq!(
            tools.iter().map(|tool| tool.name.as_ref()).collect::<Vec<_>>(),
            vec![
                "clockify_create_time_entry",
                "clockify_delete_time_entry",
                "clockify_get_time_entry",
                "clockify_get_time_report",
                "clockify_list_projects",
                "clockify_list_workspaces",
                "clockify_update_time_entry",
            ]
        );
        let read = tools
            .iter()
            .find(|tool| tool.name == "clockify_get_time_report")
            .unwrap();
        let create = tools
            .iter()
            .find(|tool| tool.name == "clockify_create_time_entry")
            .unwrap();
        let delete = tools
            .iter()
            .find(|tool| tool.name == "clockify_delete_time_entry")
            .unwrap();
        assert_eq!(
            read.annotations.as_ref().and_then(|value| value.read_only_hint),
            Some(true)
        );
        assert_eq!(
            create.annotations.as_ref().and_then(|value| value.destructive_hint),
            Some(false)
        );
        assert_eq!(
            delete.annotations.as_ref().and_then(|value| value.destructive_hint),
            Some(true)
        );
    }

    #[test]
    fn validates_provider_ids_and_report_ranges() {
        assert!(provider_id("workspace-1").is_ok());
        assert!(provider_id("../workspace").is_err());
        assert_eq!(parse_iso_duration("PT1H2M3S"), Some(3_723));
        assert_eq!(parse_iso_duration("P1DT1H"), Some(90_000));
        assert_eq!(parse_iso_duration("PT0.5S"), Some(1));
        assert_eq!(parse_iso_duration("PT"), None);
        assert_eq!(parse_iso_duration("PgarbageT1H"), None);
        assert!(duration_seconds(&serde_json::json!("1e100")).is_err());
        assert!(duration_seconds(&serde_json::json!(u64::MAX)).is_err());
        let mut oversized = fixture_core();
        oversized.start = "x".repeat(101);
        assert!(validate_core(&oversized).is_err());
    }

    #[tokio::test]
    async fn rejects_oversized_report_timezone_before_provider_request() {
        let server = MockServer::start().await;
        let result = fixture_client(&server)
            .get_time_report(GetTimeReportParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                start: "2026-10-01T00:00:00Z".into(),
                end: "2026-10-02T00:00:00Z".into(),
                time_zone: "x".repeat(101),
                group_by: GroupBy::Day,
                detail: ReportDetail::None,
                include_running: false,
            })
            .await;
        assert!(matches!(result, Err(ClockifyError::Definite(_))));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn mutation_locks_are_shared_per_entry_and_isolated_across_entries() {
        let server = MockServer::start().await;
        let client = fixture_client(&server);
        let first = client.entry_mutation_lock("workspace-1", "entry-1").await;
        let same = client.entry_mutation_lock("workspace-1", "entry-1").await;
        let different = client.entry_mutation_lock("workspace-1", "entry-2").await;
        assert!(Arc::ptr_eq(&first, &same));
        assert!(!Arc::ptr_eq(&first, &different));
    }

    #[tokio::test]
    async fn process_mutation_lock_serializes_separate_clients() {
        let server = MockServer::start().await;
        let first = fixture_client(&server);
        let second = fixture_client(&server);
        let guard = first
            .acquire_process_mutation_lock("workspace-1", "entry-1")
            .await
            .unwrap();
        let waiting = tokio::spawn(async move {
            second
                .acquire_process_mutation_lock("workspace-1", "entry-1")
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!waiting.is_finished());
        drop(guard);
        let next_guard = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        drop(next_guard);
    }

    #[tokio::test]
    async fn chunked_provider_response_stops_at_the_byte_limit() {
        let app = Router::new().route(
            "/",
            get(|| async {
                Body::from_stream(futures_util::stream::iter([
                    Ok::<_, Infallible>(vec![b'a'; 6]),
                    Ok::<_, Infallible>(vec![b'b'; 6]),
                ]))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        assert_eq!(response.content_length(), None);
        assert!(matches!(
            read_bounded_response(response, 8, false).await,
            Err(ClockifyError::Definite(_))
        ));
        server.abort();
    }

    #[test]
    fn rejects_oversized_custom_field_values_before_tool_output() {
        let mut entry = time_entry("project-1");
        entry["customFieldValues"][0]["value"] =
            serde_json::Value::String("x".repeat(MAX_CUSTOM_FIELD_STRING_BYTES + 1));
        assert!(matches!(map_time_entry(&entry, true), Err(ClockifyError::Definite(_))));
    }

    #[test]
    fn report_output_bound_matches_compact_tool_encoding() {
        let mut value = serde_json::json!({
            "result": {
                "entries": [],
                "details_truncated": false,
                "groups": (0..MAX_EXPOSED_GROUPS)
                    .map(|index| serde_json::json!({
                        "key": format!("{index}-{}", "x".repeat(3_000)),
                        "duration_seconds": index,
                    }))
                    .collect::<Vec<_>>(),
                "groups_truncated": false,
            }
        });
        bound_report_output(&mut value);
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(encoded.len() <= MAX_OUTPUT_BYTES);
        assert_eq!(value["result"]["groups_truncated"], true);
    }

    #[tokio::test]
    async fn reads_are_scoped_to_the_verified_account_and_use_the_api_key_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/workspaces"))
            .and(header("X-Api-Key", "fixture-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": "workspace-1", "name": "Fixture Workspace" }
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let value = fixture_client(&server).list_workspaces().await.unwrap();
        assert_eq!(value["connection_id"], "connection-1");
        assert_eq!(value["realm"], "global");
        assert_eq!(value["account"]["email"], "fixture@example.test");
        assert_eq!(value["result"]["items"][0]["id"], "workspace-1");
    }

    #[tokio::test]
    async fn update_preflight_scope_mismatch_stops_before_the_write() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/workspaces/workspace-1/time-entries/entry-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(time_entry("different-project")))
            .expect(1)
            .mount(&server)
            .await;

        let result = fixture_client(&server)
            .update_time_entry(UpdateTimeEntryParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                time_entry_id: "entry-1".into(),
                expected: fixture_expected(),
                replacement: fixture_core(),
            })
            .await;
        assert!(matches!(result, Err(ClockifyError::Definite(_))));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_preflight_accepts_an_existing_empty_description() {
        let server = MockServer::start().await;
        let mut current = time_entry("project-1");
        current["description"] = serde_json::Value::String(String::new());
        Mock::given(method("GET"))
            .and(path("/workspaces/workspace-1/time-entries/entry-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(current))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/workspaces/workspace-1/time-entries/entry-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(time_entry("project-1")))
            .expect(1)
            .mount(&server)
            .await;

        let mut expected = fixture_expected();
        expected.description.clear();
        let value = fixture_client(&server)
            .update_time_entry(UpdateTimeEntryParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                time_entry_id: "entry-1".into(),
                expected,
                replacement: fixture_core(),
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["delivery"], "confirmed");
    }

    #[tokio::test]
    async fn update_preflight_and_confirmation_preserve_description_whitespace() {
        let server = MockServer::start().await;
        let mut entry = time_entry("project-1");
        entry["description"] = serde_json::Value::String(" Work ".into());
        Mock::given(method("GET"))
            .and(path("/workspaces/workspace-1/time-entries/entry-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(entry.clone()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/workspaces/workspace-1/time-entries/entry-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(entry))
            .expect(1)
            .mount(&server)
            .await;

        let mut expected = fixture_expected();
        expected.description = " Work ".into();
        let mut replacement = fixture_core();
        replacement.description = " Work ".into();
        let value = fixture_client(&server)
            .update_time_entry(UpdateTimeEntryParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                time_entry_id: "entry-1".into(),
                expected,
                replacement,
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["delivery"], "confirmed");
        assert_eq!(value["result"]["entry"]["description"], " Work ");
    }

    #[tokio::test]
    async fn delete_preflight_accepts_an_existing_empty_description() {
        let server = MockServer::start().await;
        let mut current = time_entry("project-1");
        current["description"] = serde_json::Value::String(String::new());
        Mock::given(method("GET"))
            .and(path("/workspaces/workspace-1/time-entries/entry-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(current))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/workspaces/workspace-1/time-entries/entry-1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let mut expected = fixture_expected();
        expected.description.clear();
        let value = fixture_client(&server)
            .delete_time_entry(DeleteTimeEntryParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                time_entry_id: "entry-1".into(),
                expected,
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["delivery"], "confirmed");
    }

    #[tokio::test]
    async fn mutation_server_failure_reports_uncertain_delivery_without_retrying() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/workspaces/workspace-1/time-entries/entry-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(time_entry("project-1")))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/workspaces/workspace-1/time-entries/entry-1"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let value = fixture_client(&server)
            .update_time_entry(UpdateTimeEntryParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                time_entry_id: "entry-1".into(),
                expected: fixture_expected(),
                replacement: fixture_core(),
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["delivery"], "delivery_uncertain");
        assert!(value["result"]["guidance"].as_str().unwrap().contains("do not retry"));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn malformed_success_response_after_create_reports_uncertain_delivery() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/workspaces/workspace-1/user/account-1/time-entries"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let value = fixture_client(&server)
            .create_time_entry(CreateTimeEntryParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                entry: fixture_core(),
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["delivery"], "delivery_uncertain");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_confirmation_preserves_description_whitespace() {
        let server = MockServer::start().await;
        let mut entry = time_entry("project-1");
        entry["description"] = serde_json::Value::String(" Work ".into());
        Mock::given(method("POST"))
            .and(path("/workspaces/workspace-1/user/account-1/time-entries"))
            .respond_with(ResponseTemplate::new(201).set_body_json(entry))
            .expect(1)
            .mount(&server)
            .await;

        let mut core = fixture_core();
        core.description = " Work ".into();
        let value = fixture_client(&server)
            .create_time_entry(CreateTimeEntryParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                entry: core,
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["delivery"], "confirmed");
        assert_eq!(value["result"]["entry"]["description"], " Work ");
    }

    #[tokio::test]
    async fn report_without_provider_pagination_evidence_is_marked_partial() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/workspaces/workspace-1/reports/detailed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "totals": [{ "totalTime": 3600 }],
                "timeentries": [{
                    "_id": "entry-1",
                    "description": "Fixture work",
                    "taskName": "Task",
                    "timeInterval": {
                        "start": "2026-10-01T10:00:00Z",
                        "end": "2026-10-01T11:00:00Z",
                        "duration": 3600
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let value = fixture_client(&server)
            .get_time_report(GetTimeReportParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                start: "2026-10-01T00:00:00Z".into(),
                end: "2026-10-02T00:00:00Z".into(),
                time_zone: "Europe/Kyiv".into(),
                group_by: GroupBy::Day,
                detail: ReportDetail::Full,
                include_running: false,
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["completeness"], "partial");
        assert_eq!(value["result"]["reason"], "pagination_evidence_missing");
        assert_eq!(value["result"]["total_duration_seconds"], serde_json::Value::Null);
        assert_eq!(value["result"]["fetched_duration_seconds"], 3600);
        assert_eq!(value["result"]["provider_pagination"]["successful_pages"], 1);
        assert_eq!(
            value["result"]["provider_pagination"]["missing_or_unrecognized_header_pages"],
            1
        );
    }

    #[tokio::test]
    async fn empty_first_report_page_without_total_or_header_is_complete_zero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/workspaces/workspace-1/reports/detailed"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "timeentries": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let value = fixture_client(&server)
            .get_time_report(GetTimeReportParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                start: "2026-10-01T00:00:00Z".into(),
                end: "2026-10-02T00:00:00Z".into(),
                time_zone: "UTC".into(),
                group_by: GroupBy::Day,
                detail: ReportDetail::Summary,
                include_running: false,
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["completeness"], "complete");
        assert_eq!(value["result"]["reason"], serde_json::Value::Null);
        assert_eq!(value["result"]["total_duration_seconds"], 0);
        assert_eq!(value["result"]["covered_intervals"].as_array().unwrap().len(), 1);
        assert_eq!(value["result"]["missing_intervals"], serde_json::json!([]));
        assert_eq!(value["result"]["provider_pagination"]["successful_pages"], 1);
        assert_eq!(
            value["result"]["provider_pagination"]["missing_or_unrecognized_header_pages"],
            1
        );
        assert_eq!(value["result"]["provider_pagination"]["empty_page_completions"], 1);
    }

    #[tokio::test]
    async fn populated_terminal_report_without_total_remains_partial() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/workspaces/workspace-1/reports/detailed"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("last-page", "true")
                    .set_body_json(serde_json::json!({
                        "timeentries": [{
                            "_id": "entry-1",
                            "description": "Fixture work",
                            "timeInterval": {
                                "start": "2026-10-01T10:00:00Z",
                                "end": "2026-10-01T11:00:00Z",
                                "duration": 3600
                            }
                        }]
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let value = fixture_client(&server)
            .get_time_report(GetTimeReportParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                start: "2026-10-01T00:00:00Z".into(),
                end: "2026-10-02T00:00:00Z".into(),
                time_zone: "UTC".into(),
                group_by: GroupBy::Day,
                detail: ReportDetail::Full,
                include_running: false,
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["completeness"], "partial");
        assert_eq!(value["result"]["reason"], "provider_total_missing");
        assert_eq!(value["result"]["entries"].as_array().unwrap().len(), 1);
        assert_eq!(value["result"]["fetched_duration_seconds"], 3600);
        assert_eq!(value["result"]["provider_pagination"]["terminal_header_pages"], 1);
        assert_eq!(value["result"]["provider_pagination"]["missing_total_windows"], 1);
    }

    #[tokio::test]
    async fn empty_terminal_report_with_nonzero_total_is_not_complete() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/workspaces/workspace-1/reports/detailed"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("last-page", "true")
                    .set_body_json(serde_json::json!({
                        "totals": [{ "totalTime": 3600 }],
                        "timeentries": []
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let value = fixture_client(&server)
            .get_time_report(GetTimeReportParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                start: "2026-10-01T00:00:00Z".into(),
                end: "2026-10-02T00:00:00Z".into(),
                time_zone: "UTC".into(),
                group_by: GroupBy::Day,
                detail: ReportDetail::Summary,
                include_running: false,
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["completeness"], "unavailable");
        assert_eq!(value["result"]["reason"], "provider_changed");
        assert_eq!(value["result"]["total_duration_seconds"], serde_json::Value::Null);
        assert_eq!(value["result"]["covered_intervals"], serde_json::json!([]));
        assert_eq!(value["result"]["missing_intervals"].as_array().unwrap().len(), 1);
        assert_eq!(value["result"]["provider_pagination"]["terminal_header_pages"], 1);
    }

    #[tokio::test]
    async fn report_detail_none_has_stable_non_truncated_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/workspaces/workspace-1/reports/detailed"))
            .and(body_partial_json(serde_json::json!({
                "dateRangeStart": "2026-10-01T00:00:00.000000Z",
                "dateRangeEnd": "2026-10-01T23:59:59.999999Z",
                "dateRangeType": "ABSOLUTE",
                "timeZone": "UTC",
                "amountShown": "HIDE_AMOUNT",
                "users": { "ids": ["account-1"], "contains": "CONTAINS" },
                "projects": { "ids": ["project-1"], "contains": "CONTAINS" },
                "detailedFilter": { "page": 1, "pageSize": 200, "sortColumn": "DATE" },
                "sortOrder": "ASCENDING",
                "exportType": "JSON"
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("last-page", "TRUE")
                    .set_body_json(serde_json::json!({
                        "totals": [{ "totalTime": "PT1H" }],
                        "timeentries": [{
                            "_id": "entry-1",
                            "description": "Fixture work",
                            "timeInterval": {
                                "start": "2026-10-01T10:00:00Z",
                                "end": "2026-10-01T11:00:00Z",
                                "duration": "PT1H"
                            }
                        }]
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let value = fixture_client(&server)
            .get_time_report(GetTimeReportParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                start: "2026-10-01T00:00:00Z".into(),
                end: "2026-10-02T00:00:00Z".into(),
                time_zone: "UTC".into(),
                group_by: GroupBy::Day,
                detail: ReportDetail::None,
                include_running: false,
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["details_truncated"], false);
        assert_eq!(value["result"]["groups_truncated"], false);
        assert_eq!(value["result"]["entries"], serde_json::json!([]));
        assert_eq!(value["result"]["groups"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn report_provider_range_uses_clockify_utc_microseconds_and_exclusive_end() {
        let start = parse_timestamp("2026-10-01T03:00:00.000001+03:00", "start").unwrap();
        let end = parse_timestamp("2026-10-02T03:00:00.000001+03:00", "end").unwrap();
        let range = provider_report_range(start, end).unwrap();
        assert_eq!(range.0, "2026-10-01T00:00:00.000001Z");
        assert_eq!(range.1, "2026-10-02T00:00:00.000000Z");
    }

    #[test]
    fn report_provider_range_rejects_unrepresentable_nanoseconds() {
        let start = parse_timestamp("2026-10-01T00:00:00.000000001Z", "start").unwrap();
        let end = parse_timestamp("2026-10-02T00:00:00Z", "end").unwrap();
        let result = provider_report_range(start, end);
        assert!(matches!(result, Err(ClockifyError::Definite(message)) if message.contains("microsecond precision")));
    }

    #[tokio::test]
    async fn report_validation_failure_returns_only_allowlisted_diagnostics() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/workspaces/workspace-1/reports/detailed"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": 4002,
                "message": "Invalid dateRangeStart for private-project and private@example.test using secret-key"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let result = fixture_client(&server)
            .get_time_report(GetTimeReportParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                start: "2026-10-01T00:00:00Z".into(),
                end: "2026-10-02T00:00:00Z".into(),
                time_zone: "UTC".into(),
                group_by: GroupBy::Day,
                detail: ReportDetail::None,
                include_running: false,
            })
            .await;
        let Err(ClockifyError::Definite(message)) = result else {
            panic!("expected definite validation failure");
        };
        assert!(message.contains("provider_reason=invalid_value"));
        assert!(message.contains("field=date_range_start"));
        assert!(message.contains("provider_code=4002"));
        assert!(!message.contains("private-project"));
        assert!(!message.contains("private@example.test"));
        assert!(!message.contains("secret-key"));
    }

    #[tokio::test]
    async fn unknown_report_failure_does_not_echo_provider_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/workspaces/workspace-1/reports/detailed"))
            .respond_with(ResponseTemplate::new(400).set_body_string("private report content"))
            .expect(1)
            .mount(&server)
            .await;

        let result = fixture_client(&server)
            .get_time_report(GetTimeReportParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                start: "2026-10-01T00:00:00Z".into(),
                end: "2026-10-02T00:00:00Z".into(),
                time_zone: "UTC".into(),
                group_by: GroupBy::Day,
                detail: ReportDetail::None,
                include_running: false,
            })
            .await;
        let Err(ClockifyError::Definite(message)) = result else {
            panic!("expected definite validation failure");
        };
        assert_eq!(message, "Clockify request failed with status 400 Bad Request.");
        assert!(!message.contains("private report content"));
    }

    #[tokio::test]
    async fn malformed_later_report_page_preserves_partial_results() {
        let server = MockServer::start().await;
        let report_path = "/workspaces/workspace-1/reports/detailed";
        Mock::given(method("POST"))
            .and(path(report_path))
            .and(body_partial_json(
                serde_json::json!({ "detailedFilter": { "page": 1 } }),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("last-page", "false")
                    .set_body_json(serde_json::json!({
                        "totals": [{ "totalTime": 3600 }],
                        "timeentries": [{
                            "_id": "entry-1",
                            "description": "Fixture work",
                            "timeInterval": {
                                "start": "2026-10-01T10:00:00Z",
                                "end": "2026-10-01T11:00:00Z",
                                "duration": 3600
                            }
                        }]
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(report_path))
            .and(body_partial_json(
                serde_json::json!({ "detailedFilter": { "page": 2 } }),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("last-page", "true")
                    .set_body_json(serde_json::json!({ "malformed": true })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let value = fixture_client(&server)
            .get_time_report(GetTimeReportParams {
                workspace_id: "workspace-1".into(),
                project_id: "project-1".into(),
                start: "2026-10-01T00:00:00Z".into(),
                end: "2026-10-02T00:00:00Z".into(),
                time_zone: "UTC".into(),
                group_by: GroupBy::Day,
                detail: ReportDetail::Full,
                include_running: false,
            })
            .await
            .unwrap();
        assert_eq!(value["result"]["completeness"], "partial");
        assert_eq!(value["result"]["reason"], "provider_limit");
        assert_eq!(value["result"]["entries"][0]["id"], "entry-1");
        assert_eq!(value["result"]["missing_intervals"].as_array().unwrap().len(), 1);
    }
}
