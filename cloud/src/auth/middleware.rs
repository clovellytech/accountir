use axum::{
    extract::FromRequestParts,
    http::{header::AUTHORIZATION, request::Parts},
};
use axum_extra::extract::cookie::CookieJar;
use uuid::Uuid;

use crate::{
    auth::session::{lookup_session, SessionWithUser, SESSION_COOKIE_NAME},
    error::AppError,
    http::AppState,
};

/// Extracted from the request — represents the current authenticated user.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub id: Uuid,
    pub email: String,
    pub name: Option<String>,
}

impl From<SessionWithUser> for AuthenticatedUser {
    fn from(s: SessionWithUser) -> Self {
        AuthenticatedUser {
            id: s.user_id,
            email: s.email,
            name: s.name,
        }
    }
}

impl FromRequestParts<AppState> for AuthenticatedUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(parts).or_else(|| cookie_token(parts)).ok_or(AppError::Unauthorized)?;

        let session = lookup_session(&state.pool, &token)
            .await?
            .ok_or(AppError::Unauthorized)?;

        Ok(session.into())
    }
}

fn bearer_token(parts: &Parts) -> Option<String> {
    let header = parts.headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = header.strip_prefix("Bearer ").or_else(|| header.strip_prefix("bearer "))?;
    Some(token.trim().to_string())
}

fn cookie_token(parts: &Parts) -> Option<String> {
    CookieJar::from_headers(&parts.headers)
        .get(SESSION_COOKIE_NAME)
        .map(|c| c.value().to_string())
}
