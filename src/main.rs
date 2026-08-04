        use axum::{
            extract::{Form, State, WebSocketUpgrade, ws::{Message, WebSocket}},
            response::{Html, IntoResponse, Response},
            routing::{get, post},
            Router,
        };
        use futures_util::{SinkExt, StreamExt};
        use maud::{html, Markup, DOCTYPE};
        use sea_orm::{Database, DatabaseConnection};
        use serde::Deserialize;
        use std::{env, net::SocketAddr, sync::Arc};
        use tokio::sync::{RwLock, broadcast};
        use tower_http::trace::TraceLayer;
        use tracing::info;
        use uuid::Uuid;

        #[derive(Clone)]
        struct AppState {
            items: Arc<RwLock<Vec<Item>>>,
            events: broadcast::Sender<String>,
            database: Option<DatabaseConnection>,
            supabase_url: Option<String>,
        }

        #[derive(Debug, Clone)]
        struct Item { id: Uuid, title: String, summary: String }

        #[derive(Debug, Deserialize)]
        struct NewItem { title: String, #[serde(default)] summary: String }

        #[tokio::main]
        async fn main() -> Result<(), Box<dyn std::error::Error>> {
            tracing_subscriber::fmt().with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=info".into())
            ).init();
            let database = match env::var("DATABASE_URL") {
                Ok(url) => Some(Database::connect(url).await?),
                Err(_) => None,
            };
            let (events, _) = broadcast::channel(256);
            let state = AppState {
                items: Arc::new(RwLock::new(Vec::new())),
                events,
                database,
                supabase_url: env::var("SUPABASE_URL").ok(),
            };
            let app = Router::new()
                .route("/", get(index))
                .route("/healthz", get(health))
                .route("/readyz", get(health))
                .route("/fragments/items", get(items_fragment))
                .route("/items", post(create_item))
                .route("/ws", get(ws_upgrade))
                .layer(TraceLayer::new_for_http())
                .with_state(state);
            let addr: SocketAddr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into()).parse()?;
            let listener = tokio::net::TcpListener::bind(addr).await?;
            info!(%addr, "Apostille Me MASH web listening");
            axum::serve(listener, app).await?;
            Ok(())
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

        async fn create_item(State(state): State<AppState>, Form(input): Form<NewItem>) -> impl IntoResponse {
            let title = input.title.trim().to_owned();
            if title.is_empty() { return Html("<p role=alert>title is required</p>".to_owned()); }
            let item = Item { id: Uuid::new_v4(), title, summary: input.summary.chars().take(4000).collect() };
            state.items.write().await.push(item);
            let _ = state.events.send(serde_json::json!({"event_type":"record.changed"}).to_string());
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
                Ok(text) => {
                    if sender.send(Message::Text(text.into())).await.is_err() { break; }
                },
                Err(broadcast::error::RecvError::Closed) => break,
                _ => {},
            },
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Ping(data))) => {
                    if sender.send(Message::Pong(data)).await.is_err() { break; }
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {},
            }
        }
    }
}
