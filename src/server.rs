use crate::{
    app::{BbsApp, Progress, SearchRequest},
    model::SearchEvent,
};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::{Stream, StreamExt};
use rust_embed::RustEmbed;
use serde::Serialize;
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{RwLock, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::{catch_panic::CatchPanicLayer, compression::CompressionLayer, trace::TraceLayer};
use uuid::Uuid;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct Assets;

#[derive(Clone)]
struct ServerState {
    app: BbsApp,
    csrf: String,
    jobs: Arc<RwLock<HashMap<Uuid, SearchJob>>>,
}

#[derive(Clone)]
struct SearchJob {
    sender: broadcast::Sender<SearchEvent>,
    cancelled: Arc<AtomicBool>,
    history: Arc<Mutex<Vec<SearchEvent>>>,
}

#[derive(Serialize)]
struct Bootstrap {
    csrf_token: String,
    version: &'static str,
    authenticated: bool,
}

#[derive(Serialize)]
struct JobCreated {
    id: Uuid,
}

#[derive(serde::Deserialize, Default)]
struct RepositoryParams {
    #[serde(default)]
    offline: bool,
}

pub async fn serve(app: BbsApp, port: u16, open: bool) -> Result<()> {
    let state = ServerState {
        app,
        csrf: Uuid::new_v4().to_string(),
        jobs: Arc::new(RwLock::new(HashMap::new())),
    };
    let router = Router::new()
        .route("/api/v1/bootstrap", get(bootstrap))
        .route("/api/v1/repositories", get(repositories))
        .route("/api/v1/search", post(start_search))
        .route("/api/v1/search/{id}/events", get(search_events))
        .route("/api/v1/search/{id}/cancel", post(cancel_search))
        .fallback(get(static_asset))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn(local_request_guard))
        .layer(CompressionLayer::new())
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let address = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| {
            format!("cannot bind http://localhost:{port}; choose another port with --port")
        })?;
    let url = format!("http://localhost:{port}");
    println!("bbs is serving at {url}");
    if open {
        let url_to_open = url.clone();
        tokio::task::spawn_blocking(move || {
            let _ = webbrowser::open(&url_to_open);
        });
    }
    axum::serve(listener, router).await?;
    Ok(())
}

async fn local_request_guard(request: axum::extract::Request, next: Next) -> Response {
    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| {
            let hostname = host.split(':').next().unwrap_or_default();
            hostname.eq_ignore_ascii_case("localhost") || hostname == "127.0.0.1"
        });
    let origin_ok = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|origin| {
            origin.starts_with("http://localhost:") || origin.starts_with("http://127.0.0.1:")
        });
    if !host_ok || !origin_ok {
        return (StatusCode::FORBIDDEN, "local requests only").into_response();
    }
    next.run(request).await
}

async fn bootstrap(State(state): State<ServerState>) -> Json<Bootstrap> {
    Json(Bootstrap {
        csrf_token: state.csrf,
        version: env!("CARGO_PKG_VERSION"),
        authenticated: crate::auth::credentials(false).is_ok(),
    })
}

async fn repositories(
    State(state): State<ServerState>,
    Query(params): Query<RepositoryParams>,
) -> Result<Json<crate::model::RepositoryCatalog>, ApiError> {
    Ok(Json(
        state
            .app
            .catalog(params.offline, None)
            .await
            .map_err(ApiError::from)?,
    ))
}

async fn start_search(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<SearchRequest>,
) -> Result<Json<JobCreated>, ApiError> {
    verify_csrf(&state, &headers)?;
    let id = Uuid::new_v4();
    let (sender, _) = broadcast::channel(1024);
    let cancelled = Arc::new(AtomicBool::new(false));
    let history = Arc::new(Mutex::new(Vec::new()));
    state.jobs.write().await.insert(
        id,
        SearchJob {
            sender: sender.clone(),
            cancelled: cancelled.clone(),
            history: history.clone(),
        },
    );
    let app = state.app.clone();
    tokio::spawn(async move {
        let sink_sender = sender.clone();
        let progress_history = history.clone();
        let progress: Progress = Arc::new(move |event| {
            progress_history
                .lock()
                .expect("search history lock poisoned")
                .push(event.clone());
            let _ = sink_sender.send(event);
        });
        if let Err(error) = app.search(request, progress, cancelled).await {
            let event = SearchEvent::Error {
                message: error.to_string(),
            };
            history
                .lock()
                .expect("search history lock poisoned")
                .push(event.clone());
            let _ = sender.send(event);
        }
    });
    Ok(Json(JobCreated { id }))
}

async fn search_events(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let jobs = state.jobs.read().await;
    let job = jobs
        .get(&id)
        .ok_or_else(|| ApiError::not_found("search job not found"))?;
    let receiver = job.sender.subscribe();
    let history = job
        .history
        .lock()
        .expect("search history lock poisoned")
        .clone();
    let replay = tokio_stream::iter(history.into_iter().map(Ok));
    let live = BroadcastStream::new(receiver).filter_map(|item| async move {
        match item {
            Ok(event) => Some(Ok(event)),
            Err(_) => None,
        }
    });
    let stream = replay
        .chain(live)
        .map(|item: Result<SearchEvent, Infallible>| {
            item.map(|event| {
                Event::default().json_data(event).unwrap_or_else(|_| {
                    Event::default().event("error").data("serialization failed")
                })
            })
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn cancel_search(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    verify_csrf(&state, &headers)?;
    let jobs = state.jobs.read().await;
    let job = jobs
        .get(&id)
        .ok_or_else(|| ApiError::not_found("search job not found"))?;
    job.cancelled.store(true, Ordering::Relaxed);
    Ok(StatusCode::ACCEPTED)
}

fn verify_csrf(state: &ServerState, headers: &HeaderMap) -> Result<(), ApiError> {
    let supplied = headers
        .get("x-bbs-csrf")
        .and_then(|value| value.to_str().ok());
    if supplied == Some(state.csrf.as_str()) {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::FORBIDDEN,
            "invalid local session token".into(),
        ))
    }
}

async fn static_asset(uri: axum::http::Uri) -> Response {
    let requested = uri.path().trim_start_matches('/');
    let path = if requested.is_empty() {
        "index.html"
    } else {
        requested
    };
    let asset = Assets::get(path).or_else(|| Assets::get("index.html"));
    match asset {
        Some(asset) => {
            let mime = if path.ends_with(".js") {
                "text/javascript"
            } else if path.ends_with(".css") {
                "text/css"
            } else if path.ends_with(".svg") {
                "image/svg+xml"
            } else {
                "text/html; charset=utf-8"
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .body(Body::from(asset.data))
                .unwrap()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Web UI assets are not built. Run `npm --prefix web run build` and rebuild bbs.",
        )
            .into_response(),
    }
}

struct ApiError(StatusCode, String);
impl ApiError {
    fn not_found(message: &str) -> Self {
        Self(StatusCode::NOT_FOUND, message.into())
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, error.to_string())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}
