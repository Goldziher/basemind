use axum::Router;
use axum::routing::{get, post};

async fn list_users() -> &'static str {
    "[]"
}

async fn create_user() -> &'static str {
    "{}"
}

async fn show_user() -> &'static str {
    "{}"
}

pub fn app() -> Router {
    Router::new()
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}", axum::routing::get(show_user))
        .route("/health", get(|| async { "ok" }))
}
