use axum::{Router, extract::Request, routing::get};
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("", get(async |req: Request| format!("{:#?}", req)))
        .fallback_service(ServeDir::new("."));
    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}
