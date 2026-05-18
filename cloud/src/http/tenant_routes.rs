use axum::Router;

use super::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
}
