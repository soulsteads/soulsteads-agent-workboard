//! Embedded observer dashboard and authenticated agent HTTP access.
use crate::{Agent, Command, Error, Event, Store, redact_leases, webhooks::Database};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event as Sse, KeepAlive, Sse as SseResponse},
    },
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

/// How often a stream re-checks the log for writes made by other processes.
const STREAM_POLL: Duration = Duration::from_secs(1);

/// Wakes push consumers (event streams and webhook delivery) after a write, and
/// tells open streams to finish when the server shuts down.
pub struct Hub {
    wake: watch::Sender<u64>,
    closing: AtomicBool,
}
impl Hub {
    pub fn new() -> Self {
        Self {
            wake: watch::channel(0).0,
            closing: AtomicBool::new(false),
        }
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.wake.subscribe()
    }
    pub fn notify(&self) {
        self.wake.send_modify(|n| *n = n.wrapping_add(1));
    }
    pub fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.notify();
    }
    fn closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }
}
impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}
/// The caller's key, kept so that a long-lived stream can notice its revocation.
#[derive(Clone)]
struct Token(String);
#[derive(Default, Deserialize)]
struct StreamQuery {
    project: Option<String>,
    after: Option<i64>,
}
#[derive(Default, Deserialize)]
struct Browse {
    project: Option<String>,
    before: Option<i64>,
}

pub fn router(store: Store) -> Router {
    shared_router(Arc::new(Mutex::new(store)), Arc::new(Hub::new()))
}
/// Builds the application over a database and hub that other tasks, such as
/// webhook delivery, also use.
pub fn shared_router(db: Database, hub: Arc<Hub>) -> Router {
    let api = Router::new()
        .route(
            "/schema",
            get(|| async { Json(crate::mcp::command_schema()) }),
        )
        .route(
            "/session",
            get(|Extension(agent): Extension<Agent>| async move { Json(json!({"agent":agent})) }),
        )
        .route("/overview", get(overview))
        .route("/activity", get(activity))
        .route("/issues/{key}", get(issue))
        .route("/issues/{key}/events", get(issue_activity))
        .route("/issues/{key}/comments", get(comments))
        .route("/events/stream", get(stream))
        .route("/commands", post(command))
        .route_layer(middleware::from_fn_with_state(db.clone(), authenticate));
    Router::new()
        .route(
            "/",
            get(|| async {
                asset(
                    "text/html; charset=utf-8",
                    include_str!("../web/index.html"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route(
            "/styles.css",
            get(|| async { asset("text/css; charset=utf-8", include_str!("../web/styles.css")) }),
        )
        .nest("/api", api)
        .fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":{"code":"not_found","message":"route not found"}})),
            )
        })
        .with_state(db)
        .layer(Extension(hub))
        .layer(middleware::from_fn(headers))
}
fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type)], body)
}
async fn headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}
fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE,"Bearer")], Json(json!({"error":{"code":"unauthorized","message":"A valid API key for an active agent is required"}}))).into_response()
}
async fn authenticate(
    State(db): State<Database>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(token) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_owned)
    else {
        return unauthorized();
    };
    let held = token.clone();
    let result = tokio::task::spawn_blocking(move || {
        db.lock()
            .map_err(|_| ())
            .and_then(|s| s.authenticate(&token).map_err(|_| ()))
    })
    .await;
    match result {
        Ok(Ok(Some(agent))) => {
            request.extensions_mut().insert(agent);
            request.extensions_mut().insert(Token(held));
            next.run(request).await
        }
        Ok(Ok(None)) => unauthorized(),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":{"code":"internal","message":"Authentication unavailable"}})),
        )
            .into_response(),
    }
}
fn failure(error: Error) -> Response {
    let status = match error {
        Error::NotFound(_) => StatusCode::NOT_FOUND,
        Error::Invalid(_) => StatusCode::BAD_REQUEST,
        Error::Conflict(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({"error":error.json()}))).into_response()
}
async fn query(
    db: Database,
    redact: bool,
    f: impl FnOnce(&mut Store) -> Result<Value, Error> + Send + 'static,
) -> Response {
    query_replayable(db, redact, move |s| f(s).map(|v| (v, false))).await
}
/// Like `query`, marking answers that were replayed from an earlier identical request.
async fn query_replayable(
    db: Database,
    redact: bool,
    f: impl FnOnce(&mut Store) -> Result<(Value, bool), Error> + Send + 'static,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let mut store = db
            .lock()
            .map_err(|_| Error::Invalid("observer unavailable".into()))?;
        f(&mut store)
    })
    .await;
    match result {
        Ok(Ok((mut value, replayed))) => {
            if redact {
                redact_leases(&mut value);
            }
            let mut response = Json(value).into_response();
            if replayed {
                response
                    .headers_mut()
                    .insert("idempotent-replayed", HeaderValue::from_static("true"));
            }
            response
        }
        Ok(Err(error)) => failure(error),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":{"code":"internal","message":"observer unavailable"}})),
        )
            .into_response(),
    }
}
async fn overview(State(db): State<Database>, Query(q): Query<Browse>) -> Response {
    query(db, true, move |s| s.observation(q.project.as_deref())).await
}
async fn activity(State(db): State<Database>, Query(q): Query<Browse>) -> Response {
    query(db, true, move |s| {
        s.observation_activity(q.project.as_deref(), None, q.before)
    })
    .await
}
async fn issue(State(db): State<Database>, Path(key): Path<String>) -> Response {
    query(db, true, move |s| s.observation_issue(&key)).await
}
async fn issue_activity(
    State(db): State<Database>,
    Path(key): Path<String>,
    Query(q): Query<Browse>,
) -> Response {
    query(db, true, move |s| {
        s.observation_activity(None, Some(&key), q.before)
    })
    .await
}
async fn comments(
    State(db): State<Database>,
    Path(key): Path<String>,
    Query(q): Query<Browse>,
) -> Response {
    query(db, true, move |s| s.observation_comments(&key, q.before)).await
}
async fn command(
    State(db): State<Database>,
    Extension(agent): Extension<Agent>,
    Extension(hub): Extension<Arc<Hub>>,
    headers: HeaderMap,
    Json(command): Json<Command>,
) -> Response {
    if command.is_operator_only() {
        return (StatusCode::FORBIDDEN, Json(json!({"error":{"code":"forbidden","message":"Manage API keys and webhooks locally through the CLI or MCP"}}))).into_response();
    }
    let key = match headers.get("idempotency-key").map(|v| v.to_str()) {
        None => None,
        Some(Ok(key)) => Some(key.to_owned()),
        Some(Err(_)) => {
            return failure(Error::Invalid("Idempotency-Key must be plain ASCII".into()));
        }
    };
    let response = query_replayable(db, false, move |s| {
        s.execute_idempotent(&agent.id, key.as_deref(), command)
    })
    .await;
    if response.status().is_success() {
        hub.notify();
    }
    response
}

struct StreamState {
    db: Database,
    hub: Arc<Hub>,
    wake: watch::Receiver<u64>,
    token: String,
    project: Option<String>,
    cursor: i64,
    pending: VecDeque<Event>,
}
/// Server-sent events for everything that happens on the board.
///
/// A new stream carries only future events; `after` (or the `Last-Event-ID` header
/// that browsers resend on reconnect) resumes from an event id, with `after=0`
/// replaying the whole log. Each message's `id` is the event id, its `event` is the
/// action, and its `data` is the event as JSON with lease tokens removed. The
/// stream ends when the key is revoked, so clients reconnect and learn why.
async fn stream(
    State(db): State<Database>,
    Extension(hub): Extension<Arc<Hub>>,
    Extension(Token(token)): Extension<Token>,
    headers: HeaderMap,
    Query(q): Query<StreamQuery>,
) -> Response {
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    // Subscribing first means a write landing during setup still wakes the stream.
    let wake = hub.subscribe();
    let start = {
        let (db, project, after) = (db.clone(), q.project.clone(), resume.or(q.after));
        tokio::task::spawn_blocking(move || {
            db.lock()
                .map_err(|_| Error::Invalid("stream unavailable".into()))?
                .stream_cursor(project.as_deref(), after)
        })
        .await
    };
    let cursor = match start {
        Ok(Ok(cursor)) => cursor,
        Ok(Err(error)) => return failure(error),
        Err(_) => {
            return failure(Error::Invalid("stream unavailable".into()));
        }
    };
    let state = StreamState {
        db,
        hub,
        wake,
        token,
        project: q.project,
        cursor,
        pending: VecDeque::new(),
    };
    let events = futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(event) = st.pending.pop_front() {
                st.cursor = event.id;
                let mut value = serde_json::to_value(&event).unwrap_or(Value::Null);
                redact_leases(&mut value);
                let message = Sse::default()
                    .id(event.id.to_string())
                    .event(event.action)
                    .data(value.to_string());
                return Some((Ok::<_, Infallible>(message), st));
            }
            if st.hub.closing() {
                return None;
            }
            let (db, token, project, cursor) = (
                st.db.clone(),
                st.token.clone(),
                st.project.clone(),
                st.cursor,
            );
            let fetched = tokio::task::spawn_blocking(move || {
                let store = db.lock().ok()?;
                // Revocation or deactivation ends the stream on the next check.
                matches!(store.authenticate(&token), Ok(Some(_))).then_some(())?;
                store.events_after(project.as_deref(), cursor, 100).ok()
            })
            .await
            .ok()
            .flatten();
            match fetched {
                None => return None,
                Some(events) if !events.is_empty() => st.pending.extend(events),
                Some(_) => {
                    tokio::select! {
                        changed = st.wake.changed() => {
                            if changed.is_err() {
                                tokio::time::sleep(STREAM_POLL).await;
                            }
                        }
                        _ = tokio::time::sleep(STREAM_POLL) => {}
                    }
                }
            }
        }
    });
    SseResponse::new(events)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response()
}

pub async fn serve(store: Store, address: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "Agent Workboard · http://{} · API key required",
        listener.local_addr()?
    );
    let db = Arc::new(Mutex::new(store));
    let hub = Arc::new(Hub::new());
    let delivery = tokio::spawn(crate::webhooks::run(db.clone(), hub.subscribe()));
    let closing = hub.clone();
    let result = axum::serve(listener, shared_router(db, hub))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            // Open event streams would otherwise keep the server from stopping.
            closing.close();
        })
        .await;
    delivery.abort();
    result
}
