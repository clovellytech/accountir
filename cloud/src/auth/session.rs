use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use uuid::Uuid;

pub const SESSION_COOKIE_NAME: &str = "accountir_session";

/// Random 32-byte token, base64url-encoded for cookie use.
pub fn new_session_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64_url_encode(&bytes)
}

pub fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Session {
    pub id: Uuid,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

pub async fn create_session(
    pool: &PgPool,
    user_id: Uuid,
    ttl_days: i64,
    user_agent: Option<&str>,
) -> Result<(Session, String), sqlx::Error> {
    let token = new_session_token();
    let token_h = token_hash(&token);
    let expires_at = Utc::now() + Duration::days(ttl_days);

    let session: Session = sqlx::query_as(
        r#"
        INSERT INTO sessions (user_id, token_hash, expires_at, user_agent)
        VALUES ($1, $2, $3, $4)
        RETURNING id, user_id, expires_at
        "#,
    )
    .bind(user_id)
    .bind(&token_h)
    .bind(expires_at)
    .bind(user_agent)
    .fetch_one(pool)
    .await?;

    Ok((session, token))
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionWithUser {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub email: String,
    pub name: Option<String>,
    pub is_active: bool,
}

pub async fn lookup_session(
    pool: &PgPool,
    token: &str,
) -> Result<Option<SessionWithUser>, sqlx::Error> {
    let token_h = token_hash(token);
    sqlx::query_as(
        r#"
        SELECT s.id AS session_id,
               s.user_id,
               s.expires_at,
               u.email,
               u.name,
               u.is_active
        FROM sessions s
        JOIN auth_users u ON u.id = s.user_id
        WHERE s.token_hash = $1
          AND s.expires_at > now()
          AND u.is_active = true
        "#,
    )
    .bind(&token_h)
    .fetch_optional(pool)
    .await
}

pub async fn delete_session(pool: &PgPool, token: &str) -> Result<(), sqlx::Error> {
    let token_h = token_hash(token);
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(&token_h)
        .execute(pool)
        .await?;
    Ok(())
}

fn base64_url_encode(bytes: &[u8]) -> String {
    // Minimal URL-safe base64 (no padding). Avoids pulling another dep.
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4 + 2) / 3);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0b111111) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hash_is_deterministic() {
        let token = new_session_token();
        assert_eq!(token_hash(&token), token_hash(&token));
        assert_ne!(token_hash(&token), token_hash("other"));
    }

    #[test]
    fn tokens_are_unique() {
        let a = new_session_token();
        let b = new_session_token();
        assert_ne!(a, b);
    }
}
