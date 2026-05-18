use axum::{routing::get, Router};
use sqlx::PgPool;
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::config::Config;
use crate::email::EmailClient;

pub mod auth_routes;
pub mod plaid_routes;
pub mod tenant_routes;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Config,
    pub email: EmailClient,
}

pub fn router(state: AppState) -> Router {
    let trace = TraceLayer::new_for_http()
        .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
        .on_response(DefaultOnResponse::new().level(Level::INFO))
        .on_failure(DefaultOnFailure::new().level(Level::WARN));

    Router::new()
        .route("/health", get(health))
        .merge(auth_routes::router())
        .merge(tenant_routes::router())
        .merge(plaid_routes::router())
        .merge(crate::web::router())
        .layer(trace)
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
