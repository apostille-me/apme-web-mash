use axum::{
    extract::{
        ws::{Message, WebSocket},
        Form, Path, State, WebSocketUpgrade,
    },
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use maud::{html, Markup, DOCTYPE};
use next_loggers::{Logger, Options};
use sea_orm::{Database, DatabaseConnection};
use serde::Deserialize;
use std::{env, net::SocketAddr, sync::Arc};
use tokio::sync::{broadcast, Mutex, RwLock};
use tower_http::{sensitive_headers::SetSensitiveRequestHeadersLayer, trace::TraceLayer};
use tracing::info;
use uuid::Uuid;

use crate::auth::{AuthError, SharedAuthVerifier, CASES_READ_SCOPE};

mod auth;
mod data_plane;

const TENANT_HEADER: &str = "x-apme-tenant-id";

#[derive(Clone)]
struct AppState {
    items: Arc<RwLock<Vec<Item>>>,
    events: broadcast::Sender<String>,
    auth: SharedAuthVerifier,
    database: Option<DatabaseConnection>,
    http: Option<data_plane::http::Transport>,
    #[cfg(feature = "tcp-transport")]
    tcp: Option<Arc<Mutex<data_plane::tcp::Channel>>>,
    #[cfg(feature = "nats-transport")]
    nats: Option<Arc<data_plane::nats::Transport>>,
    supabase_url: Option<String>,
}

#[derive(Debug, Clone)]
struct Item {
    id: Uuid,
    title: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
struct NewItem {
    title: String,
    #[serde(default)]
    summary: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();
    let _ores_logger = init_ores_logger()?;
    let auth = SharedAuthVerifier::from_env()?;
    let database = match env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(Database::connect(url).await?),
        _ => None,
    };
    let http = data_plane::http::Transport::from_env()?;
    #[cfg(feature = "tcp-transport")]
    let tcp = match data_plane::tcp::Config::from_env()? {
        Some(config) => Some(Arc::new(Mutex::new(
            data_plane::tcp::Channel::connect(&config).await?,
        ))),
        None => None,
    };
    #[cfg(feature = "nats-transport")]
    let nats = match data_plane::nats::Config::from_env()? {
        Some(config) => Some(Arc::new(
            data_plane::nats::Transport::connect(&config).await?,
        )),
        None => None,
    };
    let (events, _) = broadcast::channel(256);
    let state = AppState {
        items: Arc::new(RwLock::new(Vec::new())),
        events,
        auth,
        database,
        http,
        #[cfg(feature = "tcp-transport")]
        tcp,
        #[cfg(feature = "nats-transport")]
        nats,
        supabase_url: env::var("SUPABASE_URL").ok(),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .route("/readyz", get(health))
        .route("/api/data-plane/{avenue}", get(data_plane_request))
        .route("/fragments/items", get(items_fragment))
        .route("/items", post(create_item))
        .route("/ws", get(ws_upgrade))
        .layer(SetSensitiveRequestHeadersLayer::new(std::iter::once(
            AUTHORIZATION,
        )))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let addr: SocketAddr = env::var("BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "Apostille Me MASH web listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn init_ores_logger() -> anyhow::Result<Logger> {
    let logger = Logger::new(Options {
        app_name: "apme-web-mash".to_owned(),
        console: true,
        ..Options::default()
    });
    logger
        .info(vec![serde_json::json!("service.starting")])
        .add_fields(serde_json::Map::from_iter([
            (
                "auth.mode".to_owned(),
                serde_json::json!("protected_introspection"),
            ),
            (
                "transport.modes".to_owned(),
                serde_json::json!([
                    "direct_read_only_db",
                    "stateless_http",
                    "stateful_mtls_tcp",
                    "durable_jetstream"
                ]),
            ),
        ]))
        .send()?;
    Ok(logger)
}

#[derive(Debug)]
struct WebError {
    status: StatusCode,
    code: &'static str,
}

impl WebError {
    const fn unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "temporarily_unavailable",
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({"error": self.code}))).into_response()
    }
}

async fn data_plane_request(
    Path(avenue): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, WebError> {
    let token = bearer(&headers)?;
    let identity = state
        .auth
        .verify_user(token, &[CASES_READ_SCOPE])
        .await
        .map_err(map_auth_error)?;
    let requested_tenant = tenant_id(&headers)?;
    if requested_tenant != identity.tenant_id {
        return Err(WebError {
            status: StatusCode::FORBIDDEN,
            code: "forbidden",
        });
    }
    let operation = match avenue.as_str() {
        "direct" => data_plane::Operation::CaseProjection,
        "http" => data_plane::Operation::ApiCases,
        "tcp" => data_plane::Operation::StatefulCases,
        "nats" => data_plane::Operation::AsyncStatus,
        _ => {
            return Err(WebError {
                status: StatusCode::NOT_FOUND,
                code: "unknown_data_plane_avenue",
            });
        }
    };
    let payload = match data_plane::choose(operation) {
        data_plane::Avenue::DirectReadOnlyDatabase => {
            let database = state.database.as_ref().ok_or_else(WebError::unavailable)?;
            data_plane::direct::case_projection(database, identity.tenant_id)
                .await
                .map_err(|_| WebError::unavailable())?
        }
        data_plane::Avenue::StatelessHttp => {
            let transport = state.http.as_ref().ok_or_else(WebError::unavailable)?;
            transport
                .cases(token, identity.tenant_id)
                .await
                .map_err(|_| WebError::unavailable())?
        }
        data_plane::Avenue::StatefulMtlsTcp => {
            #[cfg(feature = "tcp-transport")]
            {
                let transport = state.tcp.as_ref().ok_or_else(WebError::unavailable)?;
                transport
                    .lock()
                    .await
                    .cases(Uuid::new_v4(), identity.tenant_id, token)
                    .await
                    .map_err(|_| WebError::unavailable())?
            }
            #[cfg(not(feature = "tcp-transport"))]
            return Err(WebError::unavailable());
        }
        data_plane::Avenue::DurableJetStream => {
            #[cfg(feature = "nats-transport")]
            {
                let transport = state.nats.as_ref().ok_or_else(WebError::unavailable)?;
                transport
                    .status(Uuid::new_v4())
                    .await
                    .map_err(|_| WebError::unavailable())?
            }
            #[cfg(not(feature = "nats-transport"))]
            return Err(WebError::unavailable());
        }
    };
    Ok(Json(payload))
}

fn bearer(headers: &HeaderMap) -> Result<&str, WebError> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 16 * 1024
                && !value.chars().any(char::is_whitespace)
                && !value.chars().any(char::is_control)
        })
        .ok_or(WebError {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
        })
}

fn tenant_id(headers: &HeaderMap) -> Result<Uuid, WebError> {
    headers
        .get(TENANT_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or(WebError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "invalid_tenant",
        })
}

fn map_auth_error(error: AuthError) -> WebError {
    match error {
        AuthError::Unauthorized => WebError {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
        },
        AuthError::Unavailable | AuthError::Configuration => WebError::unavailable(),
    }
}

async fn health(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status":"ok",
        "service":"apme-web-mash",
        "database_configured":state.database.is_some(),
        "supabase_configured":state.supabase_url.is_some()
    }))
}

async fn index(State(state): State<AppState>) -> Html<String> {
    let count = state.items.read().await.len();
    Html(layout(count).into_string())
}

fn layout(count: usize) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width,initial-scale=1";
                title { "Apostille Me" }
                script src="https://unpkg.com/htmx.org@2.0.4" {}
                script src="https://unpkg.com/htmx-ext-ws@2.0.2" {}
                style { ("body{font-family:system-ui;max-width:72rem;margin:auto;padding:2rem;background:#f5f5f5}main{background:white;padding:2rem;border-radius:1rem}input,textarea,button{display:block;width:100%;box-sizing:border-box;margin:.5rem 0;padding:.75rem}li{padding:.6rem;border-bottom:1px solid #ddd}.muted{color:#666}") }
            }
            body hx-ext="ws" ws-connect="/ws" {
                main {
                    h1 { "Apostille Me" }
                    p { "Case operations for visa and apostille consulting." }
                    p class="muted" id="live-status" { "WebSocket connected changes will refresh the list." }
                    form hx-post="/items" hx-target="#items" hx-swap="innerHTML" {
                        label for="title" { "Title" }
                        input id="title" name="title" maxlength="256" required;
                        label for="summary" { "Summary" }
                        textarea id="summary" name="summary" maxlength="4000" {}
                        button type="submit" { "Create bootstrap record" }
                    }
                    p { "Current records: " (count) }
                    section id="items" hx-get="/fragments/items" hx-trigger="load, record-changed from:body" {}
                }
                script { ("document.body.addEventListener('htmx:wsAfterMessage',()=>document.body.dispatchEvent(new Event('record-changed')));") }
            }
        }
    }
}

async fn items_fragment(State(state): State<AppState>) -> Html<String> {
    let items = state.items.read().await.clone();
    Html(html! {
                ul { @for item in items { li data-id=(item.id) { strong { (item.title) } p { (item.summary) } } } }
            }.into_string())
}

async fn create_item(
    State(state): State<AppState>,
    Form(input): Form<NewItem>,
) -> impl IntoResponse {
    let title = input.title.trim().to_owned();
    if title.is_empty() {
        return Html("<p role=alert>title is required</p>".to_owned());
    }
    let item = Item {
        id: Uuid::new_v4(),
        title,
        summary: input.summary.chars().take(4000).collect(),
    };
    state.items.write().await.push(item);
    let _ = state
        .events
        .send(serde_json::json!({"event_type":"record.changed"}).to_string());
    items_fragment(State(state)).await
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| ws_loop(socket, state.events.subscribe()))
}

async fn ws_loop(socket: WebSocket, mut events: broadcast::Receiver<String>) {
    let (mut sender, mut receiver) = socket.split();
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(text) if sender.send(Message::Text(text.clone().into())).await.is_err() => break,
                Ok(_) => {},
                Err(broadcast::error::RecvError::Closed) => break,
                _ => {},
            },
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Ping(data))) if sender.send(Message::Pong(data.clone())).await.is_err() => break,
                Some(Ok(Message::Ping(_))) => {},
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {},
            }
        }
    }
}
