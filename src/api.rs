use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use axum_extra::extract::{
    CookieJar,
    cookie::{Cookie, SameSite},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};
use url::Url;
use uuid::Uuid;

use crate::{
    AppState,
    auth::AuthContext,
    error::AppError,
    metrics::DeviceMetrics,
    model::{EgressMode, NavigationCommand, SessionPhase, SessionSnapshot, StreamProfile},
    session::HistoryEntry,
};

const SESSION_COOKIE: &str = "xanhtab_session";
const CSRF_HEADER: &str = "x-xanhtab-csrf";

pub fn router(state: AppState) -> Router {
    let static_dir = state.config.server.static_dir.clone();
    let index = static_dir.join("index.html");
    Router::new()
        .route("/healthz", get(health))
        .route("/api/v1/status", get(public_status))
        .route("/api/v1/pair/exchange", post(exchange_pairing))
        .route("/api/v1/session", get(get_session).post(start_session))
        .route("/api/v1/session/{id}", delete(burn_session))
        .route("/api/v1/session/{id}/navigation", post(navigate))
        .route("/api/v1/session/{id}/egress", put(set_egress))
        .route(
            "/api/v1/session/{id}/stream-profile",
            put(set_stream_profile),
        )
        .route("/api/v1/metrics", get(metrics))
        .route("/api/v1/webrtc/ticket", post(issue_ticket))
        .route("/ws/v1/session/{id}/events", get(session_events))
        .fallback_service(ServeDir::new(&static_dir).not_found_service(ServeFile::new(index)))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

#[derive(Serialize)]
struct PublicStatus {
    service: &'static str,
    version: &'static str,
    pairing_available: bool,
    phase: SessionPhase,
}

async fn public_status(State(state): State<AppState>) -> Json<PublicStatus> {
    Json(PublicStatus {
        service: "xanhtab",
        version: env!("CARGO_PKG_VERSION"),
        pairing_available: state.auth.pairing_available(),
        phase: state.sessions.snapshot().await.phase,
    })
}

#[derive(Deserialize)]
struct PairRequest {
    secret: String,
}

#[derive(Serialize)]
struct PairResponse {
    client_id: Uuid,
    csrf_token: String,
    expires_in_seconds: u64,
}

async fn exchange_pairing(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(request): Json<PairRequest>,
) -> Result<(CookieJar, Json<PairResponse>), AppError> {
    validate_origin(&state, &headers)?;
    if request.secret.len() < 40 || request.secret.len() > 128 {
        return Err(AppError::InvalidPairing);
    }
    let exchange = state.auth.exchange_pairing(&request.secret)?;
    let cookie = Cookie::build((SESSION_COOKIE, exchange.session_token.to_string()))
        .path("/")
        .http_only(true)
        .secure(state.config.server.secure_cookies)
        .same_site(SameSite::Strict)
        .build();
    let response = PairResponse {
        client_id: exchange.client_id,
        csrf_token: exchange.csrf_token.to_string(),
        expires_in_seconds: exchange.expires_in_seconds,
    };
    Ok((jar.add(cookie), Json(response)))
}

#[derive(Serialize)]
struct SessionResponse {
    session: SessionSnapshot,
    history: Vec<HistoryEntry>,
}

async fn get_session(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, AppError> {
    authenticate(&state, &jar, &headers, false)?;
    Ok(Json(SessionResponse {
        session: state.sessions.snapshot().await,
        history: state.sessions.history().await,
    }))
}

#[derive(Deserialize)]
struct StartSessionRequest {
    url: Option<Url>,
}

async fn start_session(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(request): Json<StartSessionRequest>,
) -> Result<Json<SessionSnapshot>, AppError> {
    let context = authenticate(&state, &jar, &headers, true)?;
    let url = request.url.unwrap_or_else(|| {
        Url::parse(&state.config.session.initial_url).expect("validated initial URL")
    });
    validate_navigation_url(&url)?;
    Ok(Json(state.sessions.start(context.client_id, url).await?))
}

async fn burn_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<(CookieJar, Json<SessionSnapshot>), AppError> {
    let context = authenticate(&state, &jar, &headers, true)?;
    require_session_id(&state, id).await?;
    let burn_result = state.sessions.burn(context.client_id).await;
    state.auth.revoke_all();
    let pairing = state
        .auth
        .rotate_pairing()
        .map_err(|_| AppError::Internal)?;
    state
        .auth
        .write_pairing_file(
            &pairing,
            &state.config.session.pairing_file,
            &state.config.server.public_base_url,
        )
        .map_err(|_| AppError::Internal)?;
    let snapshot = burn_result?;
    let removal = Cookie::build((SESSION_COOKIE, ""))
        .path("/")
        .http_only(true)
        .secure(state.config.server.secure_cookies)
        .same_site(SameSite::Strict)
        .build();
    Ok((jar.remove(removal), Json(snapshot)))
}

async fn navigate(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(command): Json<NavigationCommand>,
) -> Result<Json<SessionSnapshot>, AppError> {
    let context = authenticate(&state, &jar, &headers, true)?;
    require_session_id(&state, id).await?;
    if let NavigationCommand::Navigate { url } = &command {
        validate_navigation_url(url)?;
    }
    Ok(Json(
        state.sessions.navigate(context.client_id, command).await?,
    ))
}

#[derive(Deserialize)]
struct EgressRequest {
    mode: EgressMode,
}

async fn set_egress(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(request): Json<EgressRequest>,
) -> Result<Json<SessionSnapshot>, AppError> {
    let context = authenticate(&state, &jar, &headers, true)?;
    require_session_id(&state, id).await?;
    Ok(Json(
        state
            .sessions
            .switch_egress(context.client_id, request.mode)
            .await?,
    ))
}

#[derive(Deserialize)]
struct StreamProfileRequest {
    profile: StreamProfile,
}

async fn set_stream_profile(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(request): Json<StreamProfileRequest>,
) -> Result<Json<SessionSnapshot>, AppError> {
    let context = authenticate(&state, &jar, &headers, true)?;
    require_session_id(&state, id).await?;
    Ok(Json(
        state
            .sessions
            .set_stream_profile(context.client_id, request.profile)
            .await?,
    ))
}

async fn metrics(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<DeviceMetrics>, AppError> {
    authenticate(&state, &jar, &headers, false)?;
    let snapshot = state.sessions.snapshot().await;
    Ok(Json(
        state
            .metrics
            .sample(snapshot.stream_profile, snapshot.egress),
    ))
}

#[derive(Serialize)]
struct TicketResponse {
    ticket: String,
    expires_in_seconds: u64,
}

async fn issue_ticket(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<TicketResponse>, AppError> {
    let context = authenticate(&state, &jar, &headers, true)?;
    let ticket = state.auth.issue_ticket(&context)?;
    Ok(Json(TicketResponse {
        ticket: ticket.to_string(),
        expires_in_seconds: state.config.session.ticket_ttl_seconds,
    }))
}

#[derive(Deserialize)]
struct EventQuery {
    ticket: String,
}

async fn session_events(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, AppError> {
    validate_origin(&state, &headers)?;
    let _context = state.auth.consume_ticket(&query.ticket)?;
    require_session_id(&state, id).await?;
    Ok(upgrade
        .on_upgrade(move |socket| event_socket(socket, state))
        .into_response())
}

async fn event_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = state.events.subscribe();
    let initial = serde_json::to_string(&serde_json::json!({
        "event": "session.snapshot",
        "session": state.sessions.snapshot().await,
    }));
    if let Ok(initial) = initial {
        if sender.send(Message::Text(initial.into())).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        let Ok(payload) = serde_json::to_string(&event) else { continue };
                        if sender.send(Message::Text(payload.into())).await.is_err() { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    }
}

fn authenticate(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    require_csrf: bool,
) -> Result<AuthContext, AppError> {
    if require_csrf {
        validate_origin(state, headers)?;
    }
    let session = jar.get(SESSION_COOKIE).map(|cookie| cookie.value());
    let csrf = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok());
    state.auth.authenticate(session, csrf, require_csrf)
}

fn validate_origin(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        if !matches!(site, "same-origin" | "same-site" | "none") {
            return Err(AppError::InvalidCsrf);
        }
    }
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        if !state
            .config
            .server
            .allowed_origins
            .iter()
            .any(|allowed| allowed == origin)
        {
            return Err(AppError::InvalidCsrf);
        }
    }
    Ok(())
}

async fn require_session_id(state: &AppState, id: Uuid) -> Result<(), AppError> {
    if state.sessions.snapshot().await.id == Some(id) {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

fn validate_navigation_url(url: &Url) -> Result<(), AppError> {
    match url.scheme() {
        "http" | "https" | "xanhtab" => Ok(()),
        _ => Err(AppError::InvalidRequest(
            "only http, https, and xanhtab URLs are allowed".into(),
        )),
    }
}

pub fn resolve_static_dir(configured: &PathBuf) -> PathBuf {
    if configured.is_absolute() {
        configured.clone()
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_script_urls() {
        assert!(validate_navigation_url(&Url::parse("javascript:alert(1)").unwrap()).is_err());
        assert!(validate_navigation_url(&Url::parse("https://example.com").unwrap()).is_ok());
    }

    #[test]
    fn cookie_name_is_stable() {
        assert_eq!(SESSION_COOKIE, "xanhtab_session");
    }

    #[test]
    fn websocket_and_mutation_origins_are_restricted() {
        let mut config = crate::config::Config::default();
        config.server.allowed_origins = vec!["http://127.0.0.1:8088".into()];
        let browser = std::sync::Arc::new(crate::browser::MockBrowser::default());
        let egress = std::sync::Arc::new(crate::netd::MockEgress::default());
        let events = crate::events::EventBus::new(4);
        let state = crate::AppState {
            config: std::sync::Arc::new(config),
            auth: crate::auth::AuthManager::new(
                std::time::Duration::from_secs(600),
                std::time::Duration::from_secs(30),
            ),
            sessions: crate::session::SessionManager::new(
                events.clone(),
                browser.clone(),
                egress.clone(),
                std::path::PathBuf::from("/tmp/xanhtab-api-test"),
                EgressMode::Direct,
                StreamProfile::Hd15,
            ),
            metrics: crate::metrics::MetricsCollector::new(crate::blocklist::Blocklist::default()),
            events,
            browser,
            egress,
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://attacker.example".parse().unwrap());
        assert!(validate_origin(&state, &headers).is_err());
    }
}
