use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    pub session_cookie_key: [u8; 64],
    pub session_ttl_days: i64,
    pub cookie_secure: bool,
    pub plaid: PlaidConfig,
    pub anthropic_api_key: Option<String>,
    pub public_base_url: String,
}

#[derive(Debug, Clone)]
pub struct PlaidConfig {
    pub client_id: String,
    pub secret: String,
    pub env: PlaidEnv,
    pub token_enc_key: [u8; 32],
    pub redirect_uri: Option<String>,
    pub webhook_url: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum PlaidEnv {
    Sandbox,
    Production,
}

impl PlaidEnv {
    pub fn base_url(self) -> &'static str {
        match self {
            PlaidEnv::Sandbox => "https://sandbox.plaid.com",
            PlaidEnv::Production => "https://production.plaid.com",
        }
    }
}

impl std::str::FromStr for PlaidEnv {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "sandbox" => Ok(PlaidEnv::Sandbox),
            "production" => Ok(PlaidEnv::Production),
            other => anyhow::bail!("PLAID_ENV must be 'sandbox' or 'production', got '{other}'"),
        }
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url = env::var("DATABASE_URL")
            .map_err(|_| anyhow::anyhow!("DATABASE_URL must be set"))?;
        let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:9877".to_string());
        let cookie_secure = env::var("COOKIE_SECURE")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let session_ttl_days = env::var("SESSION_TTL_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);

        // Cookie key: 64 random bytes, hex-encoded in env. Generate one with:
        //   openssl rand -hex 64
        let session_cookie_key_hex = env::var("SESSION_COOKIE_KEY")
            .map_err(|_| anyhow::anyhow!("SESSION_COOKIE_KEY must be set (64 hex bytes)"))?;
        let key_bytes = hex::decode(&session_cookie_key_hex)
            .map_err(|e| anyhow::anyhow!("SESSION_COOKIE_KEY must be hex: {e}"))?;
        if key_bytes.len() != 64 {
            anyhow::bail!("SESSION_COOKIE_KEY must decode to exactly 64 bytes");
        }
        let mut session_cookie_key = [0u8; 64];
        session_cookie_key.copy_from_slice(&key_bytes);

        let plaid = PlaidConfig::from_env()?;
        let anthropic_api_key = env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty());
        let public_base_url = env::var("PUBLIC_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("http://{bind_addr}"));

        Ok(Self {
            database_url,
            bind_addr,
            session_cookie_key,
            session_ttl_days,
            cookie_secure,
            plaid,
            anthropic_api_key,
            public_base_url,
        })
    }
}

impl PlaidConfig {
    fn from_env() -> anyhow::Result<Self> {
        let client_id = env::var("PLAID_CLIENT_ID")
            .map_err(|_| anyhow::anyhow!("PLAID_CLIENT_ID must be set"))?;
        let secret = env::var("PLAID_SECRET")
            .map_err(|_| anyhow::anyhow!("PLAID_SECRET must be set"))?;
        let env_str = env::var("PLAID_ENV")
            .map_err(|_| anyhow::anyhow!("PLAID_ENV must be set ('sandbox' or 'production')"))?;
        let plaid_env: PlaidEnv = env_str.parse()?;

        // Generate with: openssl rand -hex 32
        let key_hex = env::var("PLAID_TOKEN_ENC_KEY").map_err(|_| {
            anyhow::anyhow!("PLAID_TOKEN_ENC_KEY must be set (32 hex bytes for AES-256-GCM)")
        })?;
        let key_bytes = hex::decode(&key_hex)
            .map_err(|e| anyhow::anyhow!("PLAID_TOKEN_ENC_KEY must be hex: {e}"))?;
        if key_bytes.len() != 32 {
            anyhow::bail!("PLAID_TOKEN_ENC_KEY must decode to exactly 32 bytes");
        }
        let mut token_enc_key = [0u8; 32];
        token_enc_key.copy_from_slice(&key_bytes);

        let redirect_uri = env::var("PLAID_REDIRECT_URI").ok().filter(|s| !s.is_empty());
        let webhook_url = env::var("PLAID_WEBHOOK_URL").ok().filter(|s| !s.is_empty());

        Ok(Self {
            client_id,
            secret,
            env: plaid_env,
            token_enc_key,
            redirect_uri,
            webhook_url,
        })
    }
}
