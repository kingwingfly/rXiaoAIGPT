use std::{collections::HashSet, sync::Arc};

use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse as _, Response},
};
use mime_guess::MimeGuess;
use regex::Regex;
use tokio::{net::TcpListener, sync::RwLock};
use tower::ServiceBuilder;
use tower_http::services::ServeDir;

#[cfg_attr(debug_assertions, axum::debug_middleware)]
async fn find_file(
    State(state): State<Arc<RwLock<HashSet<String>>>>,
    mut req: Request,
    next: Next,
) -> Response {
    let uri = req.uri().path();
    let regex = urlencoding::decode(uri.strip_prefix('/').unwrap_or(uri)).unwrap();
    println!("regex: {}", regex);
    if regex.len() > 64 {
        return (StatusCode::BAD_REQUEST, "Too long").into_response();
    }
    let re = Regex::new(&regex).unwrap();
    {
        let state = state.read().await;
        for entry in state.iter() {
            if re.is_match(entry) {
                let uri = format!("/{}", urlencoding::encode(entry)).parse().unwrap();
                *req.uri_mut() = uri;
                drop(state);
                return next.run(req).await;
            }
        }
    }
    let mut state = state.write().await;
    for entry in walkdir::WalkDir::new(".")
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let mime = MimeGuess::from_path(entry.path()).first_or_octet_stream();
        if mime.type_() != "audio" {
            continue;
        }
        let path = entry.path().to_str().unwrap().trim_matches(['.', '/']);
        state.insert(path.to_string());
        if re.is_match(path) {
            let uri = format!("/{}", urlencoding::encode(path)).parse().unwrap();
            *req.uri_mut() = uri;
            drop(state);
            return next.run(req).await;
        }
    }
    (StatusCode::NOT_FOUND, "Music not match").into_response()
}

#[tokio::main]
async fn main() {
    let app =
        Router::new()
            .fallback_service(ServeDir::new("."))
            .layer(ServiceBuilder::new().layer(from_fn_with_state(
                Arc::new(RwLock::new(HashSet::<String>::new())),
                find_file,
            )));
    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}
