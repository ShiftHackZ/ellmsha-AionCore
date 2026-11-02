//! Fixed-origin Figma API transport shared by account verification and MCP tools.
use aionui_api_types::FigmaVerifiedIdentity;
use aionui_extension::figma::{FigmaConnectionProbe, FigmaProbeFailure};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct FigmaApi {
    http: reqwest::Client,
    base: String,
}

impl FigmaApi {
    pub(crate) fn new() -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(25))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "Figma HTTP transport unavailable".to_string())?;
        Ok(Self {
            http,
            base: "https://api.figma.com".into(),
        })
    }

    /// Paths are assembled only from validated provider IDs by the tool layer.
    /// Tokens never enter URLs, errors, tracing fields or model-visible results.
    pub(crate) async fn request(
        &self,
        token: &str,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut credential =
            reqwest::header::HeaderValue::from_str(token).map_err(|_| "Figma token is invalid".to_owned())?;
        credential.set_sensitive(true);
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("X-Figma-Token", credential)
            .query(query);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| "Figma request failed or timed out. Inspect before retrying a write.".to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "Figma denied access (401/403). Check the account, token expiry, required scope and file permissions.".into(),
                StatusCode::NOT_FOUND => "Figma resource not found or not shared with this account (404).".into(),
                StatusCode::TOO_MANY_REQUESTS => {
                    let retry = response.headers().get("retry-after").and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u64>().ok());
                    retry.map_or_else(|| "Figma rate limit reached (429). Wait before retrying.".into(), |seconds| format!("Figma rate limit reached (429). Retry after {seconds} seconds."))
                }
                _ => format!("Figma returned HTTP {}. Inspect before retrying a write.", status.as_u16()),
            });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "Figma response could not be read".to_string())?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err("Figma response exceeds 8 MiB. Request fewer nodes or a smaller depth.".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| "Figma returned invalid JSON".to_string())?;
        if value
            .get("err")
            .and_then(Value::as_str)
            .is_some_and(|error| !error.trim().is_empty())
            || value.get("error").and_then(Value::as_bool) == Some(true)
            || value
                .get("status")
                .and_then(Value::as_u64)
                .is_some_and(|status| status >= 400)
        {
            return Err("Figma reported an error. Check the resource and permissions.".into());
        }
        Ok(value)
    }

    pub(crate) async fn identity(&self, token: &str) -> Result<FigmaVerifiedIdentity, String> {
        let value = self.request(token, Method::GET, "/v1/me", &[], None).await?;
        if value.to_string().contains(token) {
            return Err("Figma returned an invalid account identity".into());
        }
        parse_identity(&value).ok_or_else(|| "Figma returned an invalid account identity".into())
    }
}

#[async_trait::async_trait]
impl FigmaConnectionProbe for FigmaApi {
    async fn verify(&self, token: &str) -> Result<FigmaVerifiedIdentity, FigmaProbeFailure> {
        self.identity(token).await.map_err(|error| {
            if error.contains("401/403") {
                FigmaProbeFailure::Unauthorized
            } else if error.contains("invalid account identity") {
                FigmaProbeFailure::InvalidIdentity
            } else {
                FigmaProbeFailure::Unavailable
            }
        })
    }
}

fn parse_identity(value: &Value) -> Option<FigmaVerifiedIdentity> {
    let text = |name: &str, max: usize| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control))
            .map(str::to_owned)
    };
    let account_id = text("id", 300)?;
    Some(FigmaVerifiedIdentity {
        account_name: text("handle", 300).unwrap_or_else(|| account_id.clone()),
        account_id,
        account_email: text("email", 320),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::{
        Router,
        extract::{Request, State},
        response::IntoResponse,
        routing::any,
    };
    use std::sync::{Arc, Mutex};
    pub(crate) type Requests = Arc<Mutex<Vec<(String, String, String)>>>;
    pub(crate) async fn fixture() -> (FigmaApi, Requests, tokio::task::JoinHandle<()>) {
        let requests: Requests = Arc::default();
        async fn handler(State(log): State<Requests>, request: Request) -> impl IntoResponse {
            let token = request
                .headers()
                .get("x-figma-token")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            let path = request.uri().to_string();
            let method = request.method().to_string();
            log.lock().unwrap().push((method, path.clone(), token.clone()));
            if token == "unauthorized" {
                return (StatusCode::FORBIDDEN, "private-provider-body-token".to_owned());
            }
            if token == "throttled" {
                return (StatusCode::TOO_MANY_REQUESTS, "private-provider-body-token".to_owned());
            }
            if token == "redirect" {
                return (StatusCode::FOUND, "private-provider-body-token".to_owned());
            }
            if path == "/v1/me" {
                return (StatusCode::OK, serde_json::json!({"id":format!("account-{}", token.strip_prefix("fixture-token-").unwrap_or("unknown")),"handle":"Fixture user","email":"user@example.test"}).to_string());
            }
            (
                StatusCode::OK,
                serde_json::json!({"name":"Fixture design","comments":[],"images":{"1:2":null}}).to_string(),
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().fallback(any(handler)).with_state(requests.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut api = FigmaApi::new().unwrap();
        api.base = format!("http://{address}");
        (api, requests, server)
    }
    #[tokio::test]
    async fn identity_uses_documented_fields_and_errors_never_echo_provider_payloads() {
        let (api, log, server) = fixture().await;
        assert_eq!(
            api.identity("fixture-token-one").await.unwrap().account_id,
            "account-one"
        );
        for token in ["unauthorized", "throttled", "redirect"] {
            let error = api.identity(token).await.unwrap_err();
            assert!(!error.contains("private-provider"));
        }
        assert_eq!(log.lock().unwrap().len(), 4);
        assert!(parse_identity(&serde_json::json!({"email":"one@example.test"})).is_none());
        assert!(parse_identity(&serde_json::json!({"id":"\n"})).is_none());
        server.abort();
    }
}

pub(crate) mod mcp;
