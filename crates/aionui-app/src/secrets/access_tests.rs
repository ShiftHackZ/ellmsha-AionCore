use super::access::*;
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    middleware::from_fn_with_state,
    routing::get,
};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;

fn request(peer: &str, host: &str) -> Request<Body> {
    let mut request = Request::builder()
        .uri("/test")
        .header("host", host)
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    request
}
#[test]
fn web_transport_requires_real_loopback_peer_and_rejects_forwarded_spoofing() {
    let mut local = request("127.0.0.1:4567", "localhost:8080");
    assert!(web_transport_allowed(&local, None));
    local
        .headers_mut()
        .insert("x-forwarded-proto", "https".parse().unwrap());
    assert!(!web_transport_allowed(&local, None));
    let mut remote = request("198.51.100.2:4567", "localhost:8080");
    remote
        .headers_mut()
        .insert("x-forwarded-proto", "https".parse().unwrap());
    assert!(!web_transport_allowed(&remote, None));
    assert!(!web_transport_allowed(&request("127.0.0.1:4567", "example.test"), None));
    let mut cross_origin = request("127.0.0.1:4567", "localhost:8080");
    cross_origin
        .headers_mut()
        .insert("origin", "https://example.test".parse().unwrap());
    assert!(!web_transport_allowed(&cross_origin, None));
    let no_peer = Request::builder()
        .header("host", "localhost")
        .body(Body::empty())
        .unwrap();
    assert!(!web_transport_allowed(&no_peer, None));
}
#[test]
fn tls_proxy_requires_private_capability_and_loopback_origin_connection() {
    let capability = "synthetic-tls-proxy-capability-000000000000000000000000000000000000";
    let digest = Sha256::digest(capability.as_bytes()).into();
    let mut proxied = request("127.0.0.1:4567", "example.test");
    proxied
        .headers_mut()
        .insert("x-forwarded-proto", "https".parse().unwrap());
    assert!(!web_transport_allowed(&proxied, Some(&digest)));
    proxied
        .headers_mut()
        .insert("x-ellmsha-secrets-tls-proxy", capability.parse().unwrap());
    assert!(web_transport_allowed(&proxied, Some(&digest)));
    proxied
        .extensions_mut()
        .insert(ConnectInfo("198.51.100.2:4567".parse::<SocketAddr>().unwrap()));
    assert!(!web_transport_allowed(&proxied, Some(&digest)));
    assert!(!web_transport_allowed(
        &request("127.0.0.1:4567", "localhost:8080"),
        Some(&digest)
    ));
}
#[tokio::test]
async fn desktop_capability_never_accepts_omitted_forged_or_runtime_credentials() {
    let capability = "synthetic-desktop-human-capability-000000000000000000000000000000";
    initialize_human_capability(capability.into());
    let app = Router::new()
        .route("/test", get(|| async { StatusCode::NO_CONTENT }))
        .layer(from_fn_with_state(HumanGuardState { local: true }, human_guard))
        .layer(axum::middleware::from_fn(aionui_secrets::routes::no_store));
    for supplied in [None, Some("forged")] {
        let mut req = request("127.0.0.1:4567", "localhost");
        if let Some(value) = supplied {
            req.headers_mut()
                .insert("x-ellmsha-secrets-human", value.parse().unwrap());
        }
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    let mut req = request("127.0.0.1:4567", "localhost");
    req.headers_mut()
        .insert("x-ellmsha-secrets-human", capability.parse().unwrap());
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::NO_CONTENT);
    let mut req = request("127.0.0.1:4567", "localhost");
    req.headers_mut()
        .insert("x-ellmsha-secrets-human", capability.parse().unwrap());
    req.headers_mut()
        .insert("x-aionui-runtime-token", "synthetic-runtime".parse().unwrap());
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::UNAUTHORIZED);
}
