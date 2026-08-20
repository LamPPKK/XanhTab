use std::{path::PathBuf, time::Duration};

use axum::{
    Json, Router,
    extract::{
        Path, State, WebSocketUpgrade,
        rejection::JsonRejection,
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
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as UpstreamMessage,
};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};
use url::Url;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{AuthContext, TicketPurpose},
    error::AppError,
    metrics::DeviceMetrics,
    model::{EgressMode, NavigationCommand, SessionPhase, SessionSnapshot, StreamProfile},
    session::HistoryEntry,
};

const SESSION_COOKIE: &str = "xanhtab_session";
const CSRF_HEADER: &str = "x-xanhtab-csrf";
const WEBSOCKET_AUTH_TIMEOUT: Duration = Duration::from_secs(5);

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
        .route("/api/v1/session/{id}/blocklist", put(set_blocklist))
        .route("/api/v1/session/{id}/auto-burn", put(set_auto_burn))
        .route("/api/v1/metrics", get(metrics))
        .route("/api/v1/webrtc/ticket", post(issue_ticket))
        .route("/ws/v1/session/{id}/events", get(session_events))
        .route("/ws/v1/session/{id}/signal", get(session_signal))
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
#[serde(deny_unknown_fields)]
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
    payload: Result<Json<PairRequest>, JsonRejection>,
) -> Result<(CookieJar, Json<PairResponse>), AppError> {
    let request = parse_json(payload)?;
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
#[serde(deny_unknown_fields)]
struct StartSessionRequest {
    url: Option<Url>,
}

async fn start_session(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    payload: Result<Json<StartSessionRequest>, JsonRejection>,
) -> Result<Json<SessionSnapshot>, AppError> {
    let request = parse_json(payload)?;
    let context = authenticate(&state, &jar, &headers, true)?;
    let url = request.url.unwrap_or_else(|| {
        Url::parse(&state.config.session.initial_url).expect("validated initial URL")
    });
    validate_navigation_url(&url)?;
    Ok(Json(state.start_session(&context, url).await?))
}

async fn burn_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<(CookieJar, Json<SessionSnapshot>), AppError> {
    let context = authenticate(&state, &jar, &headers, true)?;
    let snapshot = state.burn_controller_session(&context, id).await?;
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
    payload: Result<Json<NavigationCommand>, JsonRejection>,
) -> Result<Json<SessionSnapshot>, AppError> {
    let command = parse_json(payload)?;
    let context = authenticate(&state, &jar, &headers, true)?;
    if let NavigationCommand::Navigate { url } = &command {
        validate_navigation_url(url)?;
    }
    Ok(Json(state.navigate(&context, id, command).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EgressRequest {
    mode: EgressMode,
}

async fn set_egress(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    jar: CookieJar,
    headers: HeaderMap,
    payload: Result<Json<EgressRequest>, JsonRejection>,
) -> Result<Json<SessionSnapshot>, AppError> {
    let request = parse_json(payload)?;
    let context = authenticate(&state, &jar, &headers, true)?;
    Ok(Json(state.switch_egress(&context, id, request.mode).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamProfileRequest {
    profile: StreamProfile,
}

async fn set_stream_profile(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    jar: CookieJar,
    headers: HeaderMap,
    payload: Result<Json<StreamProfileRequest>, JsonRejection>,
) -> Result<Json<SessionSnapshot>, AppError> {
    let request = parse_json(payload)?;
    let context = authenticate(&state, &jar, &headers, true)?;
    require_session_id(&state, id).await?;
    Ok(Json(
        state
            .sessions
            .set_stream_profile(context.client_id, request.profile)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlocklistRequest {
    enabled: bool,
}

async fn set_blocklist(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    jar: CookieJar,
    headers: HeaderMap,
    payload: Result<Json<BlocklistRequest>, JsonRejection>,
) -> Result<Json<SessionSnapshot>, AppError> {
    let request = parse_json(payload)?;
    let context = authenticate(&state, &jar, &headers, true)?;
    require_session_id(&state, id).await?;
    Ok(Json(
        state
            .sessions
            .set_blocklist_enabled(context.client_id, request.enabled)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AutoBurnRequest {
    seconds: u64,
}

async fn set_auto_burn(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    jar: CookieJar,
    headers: HeaderMap,
    payload: Result<Json<AutoBurnRequest>, JsonRejection>,
) -> Result<Json<SessionSnapshot>, AppError> {
    let request = parse_json(payload)?;
    let context = authenticate(&state, &jar, &headers, true)?;
    require_session_id(&state, id).await?;
    Ok(Json(
        state
            .sessions
            .set_auto_burn_seconds(context.client_id, request.seconds)
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
    Ok(Json(state.metrics.sample(
        snapshot.stream_profile,
        snapshot.egress,
        snapshot.blocklist_enabled,
    )))
}

#[derive(Serialize)]
struct TicketResponse {
    ticket: String,
    expires_in_seconds: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TicketRequest {
    #[serde(default)]
    purpose: TicketPurpose,
}

async fn issue_ticket(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    payload: Result<Json<TicketRequest>, JsonRejection>,
) -> Result<Json<TicketResponse>, AppError> {
    let request = parse_json(payload)?;
    let context = authenticate(&state, &jar, &headers, true)?;
    state.sessions.ensure_controller(context.client_id).await?;
    let ticket = state.auth.issue_ticket(&context, request.purpose)?;
    Ok(Json(TicketResponse {
        ticket: ticket.to_string(),
        expires_in_seconds: state.config.session.ticket_ttl_seconds,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebSocketAuthFrame {
    #[serde(rename = "type")]
    kind: String,
    ticket: String,
}

async fn session_events(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, AppError> {
    validate_origin(&state, &headers)?;
    require_session_id(&state, id).await?;
    Ok(upgrade
        .on_upgrade(move |socket| event_socket(socket, state, id))
        .into_response())
}

async fn session_signal(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, AppError> {
    validate_origin(&state, &headers)?;
    if !state.config.signaling.enabled {
        return Err(AppError::SignalingFailure(
            "signaling relay is disabled".into(),
        ));
    }
    require_session_id(&state, id).await?;
    Ok(upgrade
        .on_upgrade(move |socket| signal_socket(socket, state, id))
        .into_response())
}

async fn signal_socket(mut socket: WebSocket, state: AppState, id: Uuid) {
    if let Err(error) = authorize_websocket(&mut socket, &state, id, TicketPurpose::Signaling).await
    {
        reject_websocket(&mut socket, &error).await;
        return;
    }
    let upstream = match connect_async(&state.config.signaling.upstream_uri).await {
        Ok((upstream, _)) => upstream,
        Err(error) => {
            reject_websocket(&mut socket, &AppError::SignalingFailure(error.to_string())).await;
            return;
        }
    };
    relay_signaling(socket, upstream).await;
}

async fn relay_signaling(
    socket: WebSocket,
    upstream: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
) {
    let (mut client_sender, mut client_receiver) = socket.split();
    let (mut upstream_sender, mut upstream_receiver) = upstream.split();
    loop {
        tokio::select! {
            client = client_receiver.next() => {
                let Some(Ok(message)) = client else { break };
                let Some(message) = to_upstream_message(message) else { continue };
                let closing = matches!(message, UpstreamMessage::Close(_));
                if upstream_sender.send(message).await.is_err() || closing { break; }
            }
            upstream = upstream_receiver.next() => {
                let Some(Ok(message)) = upstream else { break };
                let Some(message) = to_client_message(message) else { continue };
                let closing = matches!(message, Message::Close(_));
                if client_sender.send(message).await.is_err() || closing { break; }
            }
        }
    }
    let _ = upstream_sender.close().await;
    let _ = client_sender.close().await;
}

fn to_upstream_message(message: Message) -> Option<UpstreamMessage> {
    match message {
        Message::Text(value) => Some(UpstreamMessage::Text(value.to_string().into())),
        Message::Binary(value) => Some(UpstreamMessage::Binary(value.to_vec().into())),
        Message::Ping(value) => Some(UpstreamMessage::Ping(value.to_vec().into())),
        Message::Pong(value) => Some(UpstreamMessage::Pong(value.to_vec().into())),
        Message::Close(_) => Some(UpstreamMessage::Close(None)),
    }
}

fn to_client_message(message: UpstreamMessage) -> Option<Message> {
    match message {
        UpstreamMessage::Text(value) => Some(Message::Text(value.to_string().into())),
        UpstreamMessage::Binary(value) => Some(Message::Binary(value.to_vec().into())),
        UpstreamMessage::Ping(value) => Some(Message::Ping(value.to_vec().into())),
        UpstreamMessage::Pong(value) => Some(Message::Pong(value.to_vec().into())),
        UpstreamMessage::Close(_) => Some(Message::Close(None)),
        UpstreamMessage::Frame(_) => None,
    }
}

async fn event_socket(mut socket: WebSocket, state: AppState, id: Uuid) {
    if let Err(error) = authorize_websocket(&mut socket, &state, id, TicketPurpose::Events).await {
        reject_websocket(&mut socket, &error).await;
        return;
    }
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
                        let close_after_send = matches!(
                            event.session.phase,
                            SessionPhase::Burning | SessionPhase::Idle
                        );
                        let Ok(payload) = serde_json::to_string(&event) else { continue };
                        if sender.send(Message::Text(payload.into())).await.is_err() { break; }
                        if close_after_send { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    }
}

async fn authorize_websocket(
    socket: &mut WebSocket,
    state: &AppState,
    id: Uuid,
    purpose: TicketPurpose,
) -> Result<(), AppError> {
    let incoming = tokio::time::timeout(WEBSOCKET_AUTH_TIMEOUT, socket.recv())
        .await
        .map_err(|_| AppError::Unauthorized)?
        .ok_or(AppError::Unauthorized)?
        .map_err(|_| AppError::Unauthorized)?;
    let Message::Text(payload) = incoming else {
        return Err(AppError::Unauthorized);
    };
    if payload.len() > 256 {
        return Err(AppError::Unauthorized);
    }
    let frame: WebSocketAuthFrame =
        serde_json::from_str(&payload).map_err(|_| AppError::Unauthorized)?;
    if frame.kind != "authenticate" || frame.ticket.len() < 40 || frame.ticket.len() > 128 {
        return Err(AppError::Unauthorized);
    }
    let context = state.auth.consume_ticket(&frame.ticket, purpose)?;
    require_session_id(state, id).await?;
    state.sessions.ensure_controller(context.client_id).await?;
    socket
        .send(Message::Text(
            serde_json::json!({ "type": "authenticated" })
                .to_string()
                .into(),
        ))
        .await
        .map_err(|_| AppError::Unauthorized)
}

async fn reject_websocket(socket: &mut WebSocket, error: &AppError) {
    let code = match error {
        AppError::SignalingFailure(_) => "SIGNALING_UNAVAILABLE",
        _ => "AUTH_REQUIRED",
    };
    let _ = socket
        .send(Message::Text(
            serde_json::json!({ "error": { "code": code } })
                .to_string()
                .into(),
        ))
        .await;
    let _ = socket.send(Message::Close(None)).await;
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

fn parse_json<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, AppError> {
    payload
        .map(|Json(value)| value)
        .map_err(|_| AppError::InvalidRequest("malformed JSON body".into()))
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
                1_800,
            ),
            metrics: crate::metrics::MetricsCollector::new(crate::blocklist::Blocklist::default()),
            events,
            browser,
            egress,
            lifecycle: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://attacker.example".parse().unwrap());
        assert!(validate_origin(&state, &headers).is_err());
    }
}
