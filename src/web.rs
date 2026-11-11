//! Embedded observer dashboard and authenticated agent HTTP access.
use crate::{Agent, Command, Error, Store};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

type Database = Arc<Mutex<Store>>;
#[derive(Default, Deserialize)]
struct Browse {
    project: Option<String>,
    before: Option<i64>,
}

pub fn router(store: Store) -> Router {
    let db = Arc::new(Mutex::new(store));
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
    let result = tokio::task::spawn_blocking(move || {
        db.lock()
            .map_err(|_| ())
            .and_then(|s| s.authenticate(&token).map_err(|_| ()))
    })
    .await;
    match result {
        Ok(Ok(Some(agent))) => {
            request.extensions_mut().insert(agent);
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
/// Leases are displayed by expiry, never by their worker fencing credential.
fn redact(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Object(lease)) = object.get_mut("lease") {
                lease.remove("token");
            }
            for child in object.values_mut() {
                redact(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                redact(child);
            }
        }
        _ => {}
    }
}
async fn query(
    db: Database,
    redact_leases: bool,
    f: impl FnOnce(&mut Store) -> Result<Value, Error> + Send + 'static,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let mut store = db
            .lock()
            .map_err(|_| Error::Invalid("observer unavailable".into()))?;
        f(&mut store)
    })
    .await;
    match result {
        Ok(Ok(mut value)) => {
            if redact_leases {
                redact(&mut value);
            }
            Json(value).into_response()
        }
        Ok(Err(error)) => {
            let status = match error {
                Error::NotFound(_) => StatusCode::NOT_FOUND,
                Error::Invalid(_) => StatusCode::BAD_REQUEST,
                Error::Conflict(_) => StatusCode::CONFLICT,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({"error":error.json()}))).into_response()
        }
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
    Json(command): Json<Command>,
) -> Response {
    if command.is_key_management() {
        return (StatusCode::FORBIDDEN, Json(json!({"error":{"code":"forbidden","message":"Manage API keys locally through the CLI or MCP"}}))).into_response();
    }
    query(db, false, move |s| s.execute(&agent.id, command)).await
}

pub async fn serve(store: Store, address: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!(
        "Agent Workboard · http://{} · API key required",
        listener.local_addr()?
    );
    axum::serve(listener, router(store))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
