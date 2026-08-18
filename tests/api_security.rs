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
    auth::AuthManager,
    blocklist::Blocklist,
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
    let temp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.static_dir = temp.path().join("web");
    config.session.runtime_dir = temp.path().join("session");
    config.session.pairing_file = temp.path().join("pairing.txt");
    let config = Arc::new(config);
    let browser: Arc<dyn BrowserBackend> = Arc::new(MockBrowser::default());
    let egress: Arc<dyn EgressBackend> = Arc::new(MockEgress::default());
    let events = EventBus::new(32);
    let sessions = SessionManager::new(
        events.clone(),
        browser.clone(),
        egress.clone(),
        config.session.runtime_dir.clone(),
        config.network.initial_mode,
        config.session.initial_profile,
        config.session.auto_burn_seconds,
    );
    let auth = AuthManager::new(Duration::from_secs(600), Duration::from_secs(30));
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
        metrics: MetricsCollector::new(Blocklist::default()),
        events,
        browser,
        egress,
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
    let response = harness
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
