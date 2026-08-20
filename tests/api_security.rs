use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use xanhtab::{
    AppState, api,
    auth::{AuthManager, TicketPurpose},
    blocklist::{Blocklist, compile_hosts},
    browser::{BrowserBackend, MockBrowser},
    config::Config,
    events::EventBus,
    metrics::MetricsCollector,
    netd::{EgressBackend, MockEgress},
    session::SessionManager,
};

struct Harness {
    app: axum::Router,
    state: AppState,
    secret: String,
    _temp: TempDir,
}

fn harness() -> Harness {
    harness_with_auth_ttl(Duration::from_secs(600))
}

fn harness_with_auth_ttl(auth_ttl: Duration) -> Harness {
    harness_with_auth_ttl_and_hosts(auth_ttl, None)
}

fn harness_with_blocked_hosts(hosts: &str) -> Harness {
    harness_with_auth_ttl_and_hosts(Duration::from_secs(600), Some(hosts))
}

fn harness_with_auth_ttl_and_hosts(auth_ttl: Duration, hosts: Option<&str>) -> Harness {
    let temp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.static_dir = temp.path().join("web");
    config.session.runtime_dir = temp.path().join("session");
    config.session.pairing_file = temp.path().join("pairing.txt");
    let config = Arc::new(config);
    let browser: Arc<dyn BrowserBackend> = Arc::new(MockBrowser::default());
    let egress: Arc<dyn EgressBackend> = Arc::new(MockEgress::default());
    let events = EventBus::new(32);
    let blocklist = if let Some(hosts) = hosts {
        let source = temp.path().join("hosts.txt");
        let compiled = temp.path().join("blocklist.fst");
        std::fs::write(&source, hosts).unwrap();
        compile_hosts(&[source], &compiled).unwrap();
        Blocklist::open(compiled).unwrap()
    } else {
        Blocklist::default()
    };
    let sessions = SessionManager::new_with_blocklist(
        events.clone(),
        browser.clone(),
        egress.clone(),
        config.session.runtime_dir.clone(),
        config.network.initial_mode,
        config.session.initial_profile,
        config.session.auto_burn_seconds,
        blocklist.clone(),
    );
    let auth = AuthManager::new(auth_ttl, Duration::from_secs(30));
    let pairing = auth.rotate_pairing().unwrap();
    let secret = pairing.secret.to_string();
    auth.write_pairing_file(
        &pairing,
        &config.session.pairing_file,
        &config.server.public_base_url,
    )
    .unwrap();
    let state = AppState {
        config,
        auth,
        sessions,
        metrics: MetricsCollector::new(blocklist),
        events,
        browser,
        egress,
        lifecycle: Arc::new(tokio::sync::Mutex::new(())),
    };
    Harness {
        app: api::router(state.clone()),
        state,
        secret,
        _temp: temp,
    }
}

async fn body_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

async fn pair(harness: &Harness) -> (String, String) {
    pair_with_secret(harness, &harness.secret).await
}

async fn pair_with_secret(harness: &Harness, secret: &str) -> (String, String) {
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/pair/exchange")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "secret": secret }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let payload = body_json(response).await;
    (cookie, payload["csrf_token"].as_str().unwrap().to_string())
}

fn pairing_secret(path: &std::path::Path) -> String {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .find_map(|line| {
            line.strip_prefix("PAIRING_URL=")
                .and_then(|url| url.split_once("#pair="))
                .map(|(_, secret)| secret.to_string())
        })
        .expect("pairing file contains a fragment secret")
}

#[tokio::test]
async fn pairing_is_single_use_and_mutations_require_csrf() {
    let harness = harness();
    let (cookie, csrf) = pair(&harness).await;

    let replay = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/pair/exchange")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({ "secret": harness.secret }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);

    let missing_csrf = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);

    let started = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
}

#[tokio::test]
async fn malformed_json_uses_a_stable_redacted_error() {
    let harness = harness();
    let response = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/pair/exchange")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"secret":}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let payload = body_json(response).await;
    assert_eq!(payload["error"]["code"], "REQUEST_INVALID");
    assert_eq!(
        payload["error"]["message"],
        "invalid request: malformed JSON body"
    );
}

#[tokio::test]
async fn expired_auth_burns_session_and_publishes_fresh_pairing() {
    let harness = harness_with_auth_ttl(Duration::from_secs(1));
    let original_secret = harness.secret.clone();
    let (cookie, csrf) = pair(&harness).await;
    let session_token = cookie
        .split_once('=')
        .map(|(_, value)| value)
        .expect("session cookie has a value");
    let stale_context = harness
        .state
        .auth
        .authenticate(Some(session_token), None, false)
        .unwrap();
    let started = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    std::fs::create_dir_all(&harness.state.config.session.runtime_dir).unwrap();
    std::fs::write(
        harness.state.config.session.runtime_dir.join("cookie.db"),
        "secret",
    )
    .unwrap();

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(harness.state.recover_expired_auth().await.unwrap());
    assert_eq!(
        harness.state.sessions.snapshot().await.phase,
        xanhtab::model::SessionPhase::Idle
    );
    assert_eq!(
        std::fs::read_dir(&harness.state.config.session.runtime_dir)
            .unwrap()
            .count(),
        0
    );

    let stale = harness
        .app
        .clone()
        .oneshot(
            Request::get("/api/v1/session")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
    assert!(matches!(
        harness
            .state
            .start_session(
                &stale_context,
                url::Url::parse("https://example.com/stale").unwrap(),
            )
            .await,
        Err(xanhtab::error::AppError::Unauthorized)
    ));
    assert!(!harness.state.recover_expired_auth().await.unwrap());

    let status = harness
        .app
        .clone()
        .oneshot(Request::get("/api/v1/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(body_json(status).await["pairing_available"], true);

    let fresh_secret = pairing_secret(&harness.state.config.session.pairing_file);
    assert_ne!(fresh_secret, original_secret);
    let _ = pair_with_secret(&harness, &fresh_secret).await;
}

#[tokio::test]
async fn burn_clears_runtime_and_revokes_cookie() {
    let harness = harness();
    let (cookie, csrf) = pair(&harness).await;
    let started = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let payload = body_json(started).await;
    let id = payload["id"].as_str().unwrap();
    let ticket_response = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/webrtc/ticket")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from(r#"{"purpose":"signaling"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ticket_response.status(), StatusCode::OK);
    let ticket = body_json(ticket_response).await["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::create_dir_all(&harness.state.config.session.runtime_dir).unwrap();
    std::fs::write(
        harness.state.config.session.runtime_dir.join("cookie.db"),
        "secret",
    )
    .unwrap();

    let burned = harness
        .app
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/session/{id}"))
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(burned.status(), StatusCode::OK);
    assert_eq!(
        std::fs::read_dir(&harness.state.config.session.runtime_dir)
            .unwrap()
            .count(),
        0
    );

    let stale = harness
        .app
        .clone()
        .oneshot(
            Request::get("/api/v1/session")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
    assert!(
        harness
            .state
            .auth
            .consume_ticket(&ticket, TicketPurpose::Signaling)
            .is_err()
    );
}

#[tokio::test]
async fn session_policy_endpoints_require_controller_and_csrf() {
    let harness = harness();
    let (cookie, csrf) = pair(&harness).await;
    let started = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let id = body_json(started).await["id"].as_str().unwrap().to_string();

    let blocklist = harness
        .app
        .clone()
        .oneshot(
            Request::put(format!("/api/v1/session/{id}/blocklist"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from(r#"{"enabled":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(blocklist.status(), StatusCode::OK);
    assert_eq!(body_json(blocklist).await["blocklist_enabled"], false);

    let without_csrf = harness
        .app
        .clone()
        .oneshot(
            Request::put(format!("/api/v1/session/{id}/auto-burn"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .body(Body::from(r#"{"seconds":300}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(without_csrf.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn blocklist_policy_rejects_navigation_and_preserves_session_state() {
    let harness = harness_with_blocked_hosts("0.0.0.0 blocked.example\n");
    let (cookie, csrf) = pair(&harness).await;

    let blocked_start = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from(r#"{"url":"https://blocked.example/"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(blocked_start.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(blocked_start).await["error"]["code"],
        "NAVIGATION_BLOCKED"
    );
    assert_eq!(
        harness.state.sessions.snapshot().await.phase,
        xanhtab::model::SessionPhase::Idle
    );
    assert!(harness.state.sessions.history().await.is_empty());

    let started = harness
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from(r#"{"url":"https://example.com/"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let id = body_json(started).await["id"].as_str().unwrap().to_string();

    let blocked_navigation = harness
        .app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/session/{id}/navigation"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from(
                    r#"{"navigate":{"url":"https://pixel.blocked.example/path"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(blocked_navigation.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(blocked_navigation).await["error"]["code"],
        "NAVIGATION_BLOCKED"
    );
    let unchanged = harness.state.sessions.snapshot().await;
    assert_eq!(unchanged.url.unwrap().as_str(), "https://example.com/");
    assert_eq!(harness.state.sessions.history().await.len(), 1);
    assert_eq!(
        harness
            .state
            .metrics
            .sample(
                unchanged.stream_profile,
                unchanged.egress,
                unchanged.blocklist_enabled,
            )
            .blocked_requests,
        2
    );

    let disabled = harness
        .app
        .clone()
        .oneshot(
            Request::put(format!("/api/v1/session/{id}/blocklist"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from(r#"{"enabled":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(disabled.status(), StatusCode::OK);

    let allowed_by_policy = harness
        .app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/session/{id}/navigation"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &cookie)
                .header("x-xanhtab-csrf", &csrf)
                .body(Body::from(
                    r#"{"navigate":{"url":"https://blocked.example/allowed"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allowed_by_policy.status(), StatusCode::OK);
    assert_eq!(harness.state.sessions.history().await.len(), 2);
}
