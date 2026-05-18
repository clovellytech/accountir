use axum::{
    extract::State,
    http::{header::USER_AGENT, HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    auth::{
        password::{hash_password, verify_password},
        session::{create_session, delete_session, SESSION_COOKIE_NAME},
        AuthenticatedUser,
    },
    error::{AppError, AppResult},
    http::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/signup", post(signup))
        .route("/auth/register", post(signup))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/api/me", get(me))
}

#[derive(Debug, Deserialize)]
pub struct SignupRequest {
    pub email: String,
    pub password: String,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: Uuid,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub id: Uuid,
    pub email: String,
    pub name: Option<String>,
    /// Session token, also set as a cookie. Non-browser clients use this as a Bearer token.
    pub api_key: String,
}

async fn signup(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(req): Json<SignupRequest>,
) -> AppResult<(StatusCode, CookieJar, Json<AuthResponse>)> {
    validate_email(&req.email)?;
    validate_password(&req.password)?;
    let normalized = normalize_email(&req.email);

    let password_hash =
        hash_password(&req.password).map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;

    let mut tx = state.pool.begin().await?;

    let user: UserRow = sqlx::query_as(
        r#"
        INSERT INTO auth_users (email, email_normalized, password_hash, name)
        VALUES ($1, $2, $3, $4)
        RETURNING id, email, name
        "#,
    )
    .bind(&req.email)
    .bind(&normalized)
    .bind(&password_hash)
    .bind(&req.name)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_unique_violation)?;

    create_personal_company(&mut tx, &user).await?;

    tx.commit().await?;

    let (jar, token) = issue_session_cookie(jar, &state, user.id, user_agent(&headers)).await?;
    Ok((
        StatusCode::CREATED,
        jar,
        Json(AuthResponse {
            id: user.id,
            email: user.email,
            name: user.name,
            api_key: token,
        }),
    ))
}

async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> AppResult<(CookieJar, Json<AuthResponse>)> {
    let normalized = normalize_email(&req.email);

    let row: Option<UserWithHashRow> = sqlx::query_as(
        r#"
        SELECT id, email, name, password_hash
        FROM auth_users
        WHERE email_normalized = $1 AND is_active = true
        "#,
    )
    .bind(&normalized)
    .fetch_optional(&state.pool)
    .await?;

    let Some(user) = row else {
        return Err(AppError::Unauthorized);
    };

    let ok = verify_password(&req.password, &user.password_hash)
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
    if !ok {
        return Err(AppError::Unauthorized);
    }

    let (jar, token) = issue_session_cookie(jar, &state, user.id, user_agent(&headers)).await?;
    Ok((
        jar,
        Json(AuthResponse {
            id: user.id,
            email: user.email,
            name: user.name,
            api_key: token,
        }),
    ))
}

async fn logout(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<(CookieJar, StatusCode)> {
    if let Some(cookie) = jar.get(SESSION_COOKIE_NAME) {
        delete_session(&state.pool, cookie.value()).await?;
    }
    let jar = jar.remove(Cookie::from(SESSION_COOKIE_NAME));
    Ok((jar, StatusCode::NO_CONTENT))
}

async fn me(user: AuthenticatedUser) -> Json<UserResponse> {
    Json(UserResponse {
        id: user.id,
        email: user.email,
        name: user.name,
    })
}

#[derive(Debug, sqlx::FromRow)]
struct UserRow {
    id: Uuid,
    email: String,
    name: Option<String>,
}

impl From<UserRow> for UserResponse {
    fn from(u: UserRow) -> Self {
        Self {
            id: u.id,
            email: u.email,
            name: u.name,
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct UserWithHashRow {
    id: Uuid,
    email: String,
    name: Option<String>,
    password_hash: String,
}

fn user_agent(headers: &HeaderMap) -> Option<&str> {
    headers.get(USER_AGENT).and_then(|v| v.to_str().ok())
}

async fn issue_session_cookie(
    jar: CookieJar,
    state: &AppState,
    user_id: Uuid,
    ua: Option<&str>,
) -> Result<(CookieJar, String), AppError> {
    let (_session, token) =
        create_session(&state.pool, user_id, state.config.session_ttl_days, ua).await?;

    let cookie = Cookie::build((SESSION_COOKIE_NAME, token.clone()))
        .http_only(true)
        .secure(state.config.cookie_secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::days(state.config.session_ttl_days))
        .build();

    Ok((jar.add(cookie), token))
}

fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

fn validate_email(email: &str) -> Result<(), AppError> {
    let trimmed = email.trim();
    if trimmed.is_empty() || !trimmed.contains('@') || trimmed.len() > 320 {
        return Err(AppError::BadRequest("invalid email".into()));
    }
    Ok(())
}

fn validate_password(pw: &str) -> Result<(), AppError> {
    if pw.len() < 8 {
        return Err(AppError::BadRequest(
            "password must be at least 8 characters".into(),
        ));
    }
    if pw.len() > 1024 {
        return Err(AppError::BadRequest("password too long".into()));
    }
    Ok(())
}

fn map_unique_violation(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db_err) = &e {
        if db_err.code().as_deref() == Some("23505") {
            return AppError::Conflict("email already registered".into());
        }
    }
    AppError::Database(e)
}

/// Create a default "personal" company for a brand-new user and add them as owner.
/// Without this, /plaid/* calls would 403 because there's no membership to resolve.
async fn create_personal_company(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: &UserRow,
) -> Result<Uuid, AppError> {
    let display_name = user.name.clone().unwrap_or_else(|| {
        user.email
            .split('@')
            .next()
            .unwrap_or("Personal")
            .to_string()
    });
    let slug_base = slugify(&display_name);
    let slug = format!("{}-{}", slug_base, &Uuid::new_v4().simple().to_string()[..8]);

    let row: (Uuid,) = sqlx::query_as(
        r#"
        INSERT INTO companies (slug, name, owner_user_id)
        VALUES ($1, $2, $3)
        RETURNING id
        "#,
    )
    .bind(&slug)
    .bind(&display_name)
    .bind(user.id)
    .fetch_one(&mut **tx)
    .await?;
    let company_id = row.0;

    sqlx::query(
        r#"
        INSERT INTO memberships (user_id, company_id, role)
        VALUES ($1, $2, 'owner')
        "#,
    )
    .bind(user.id)
    .bind(company_id)
    .execute(&mut **tx)
    .await?;

    Ok(company_id)
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    let slugged = if trimmed.is_empty() { "user" } else { trimmed };
    // companies.slug regex max length is 40; reserve 9 for the `-<8hex>` suffix.
    slugged.chars().take(31).collect()
}
